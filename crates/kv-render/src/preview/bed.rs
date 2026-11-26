//! The preview's audio bed: the section's music, with the clip audio of the
//! media whose `clip_audio` is on mixed under it exactly as export does
//! ([`kv_audio::render_clip_audio`], then [`kv_audio::mix_under_music`]).
//!
//! - **No clip audio**: the music bed (rendered at 100 %) is handed to the
//!   transport at once and the music volume is the transport's live gain,
//!   as before clip audio existed.
//! - **Clip audio**: rendering it decodes every segment, so it runs on a
//!   background thread ("kv-preview-bed"); the transport keeps playing the
//!   previous bed until the new one replaces it in one swap
//!   ([`AudioTransport::set_bed_and_volume`]), which leaves the play state
//!   and the clock alone: no gap, no jump.
//! - **Music volume** (decision): the mix is rebuilt as
//!   `mix_under_music(music * volume, clip)` at transport gain 1, so the
//!   volume scales only the music and the limiter sees what export's does.
//!   The rendered clip audio is cached by (edit, sources, length), so a
//!   volume change only re-mixes (no decode).
//! - A newer request supersedes older ones: a superseded job stops at its
//!   next check and never touches the transport.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use kv_audio::{mix_under_music, render_clip_audio, ClipAudioOptions, ClipAudioSource, PcmAudio};
use kv_core::{Edl, Micros};

use super::{lock, AudioTransport};

/// What the clip audio is rendered from; equal keys give equal audio.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ClipKey {
    pub edl: Edl,
    /// The edit's media with clip audio on, with resolved paths.
    pub sources: Vec<ClipAudioSource>,
    /// Section length.
    pub length: Micros,
}

impl ClipKey {
    /// The key for `edl`, or `None` when no source has clip audio on.
    pub fn new(
        edl: &Edl,
        sources: Vec<(kv_core::MediaId, PathBuf)>,
        length: Micros,
    ) -> Option<Self> {
        if sources.is_empty() {
            return None;
        }
        Some(ClipKey {
            edl: edl.clone(),
            sources: sources
                .into_iter()
                .map(|(media_id, path)| ClipAudioSource { media_id, path })
                .collect(),
            length,
        })
    }
}

#[derive(Default)]
struct State {
    /// Generation of the latest request.
    requested: u64,
    /// Generation whose bed the transport holds.
    applied: u64,
    clip: Option<(ClipKey, Arc<PcmAudio>)>,
}

#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    applied: Condvar,
}

/// Builds beds for one transport. See the module docs.
#[derive(Default)]
pub(super) struct BedMixer {
    inner: Arc<Inner>,
}

impl BedMixer {
    /// Give `audio` the bed for `music` (rendered at 100 %, `None` for no
    /// music) at linear `volume`, with the clip audio of `clip` mixed under
    /// it. Returns at once; with clip audio the bed is swapped in later.
    pub fn update(
        &self,
        audio: &Arc<AudioTransport>,
        music: Option<Arc<PcmAudio>>,
        volume: f32,
        clip: Option<ClipKey>,
    ) {
        let mut st = lock(&self.inner.state);
        st.requested += 1;
        let generation = st.requested;
        let Some(key) = clip else {
            apply(&self.inner, &mut st, generation, audio, music, volume);
            return;
        };
        drop(st);
        let inner = Arc::clone(&self.inner);
        let audio = Arc::clone(audio);
        let fallback = (Arc::clone(&inner), Arc::clone(&audio), music.clone());
        let spawned = std::thread::Builder::new()
            .name("kv-preview-bed".into())
            .spawn(move || mix(&inner, generation, &audio, music, volume, key));
        if let Err(e) = spawned {
            tracing::warn!("preview: cannot start the clip audio render: {e}");
            let (inner, audio, music) = fallback;
            let mut st = lock(&inner.state);
            apply(&inner, &mut st, generation, &audio, music, volume);
        }
    }

    /// Wait until the transport holds the bed of the latest
    /// [`update`](Self::update), up to `timeout`. False on timeout.
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = lock(&self.inner.state);
        while st.applied < st.requested {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            st = self
                .inner
                .applied
                .wait_timeout(st, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        true
    }
}

/// Hand `bed` at `gain` to `audio` if `generation` is still the latest.
fn apply(
    inner: &Inner,
    st: &mut State,
    generation: u64,
    audio: &AudioTransport,
    bed: Option<Arc<PcmAudio>>,
    gain: f32,
) {
    if st.requested != generation {
        return;
    }
    if let Err(e) = audio.set_bed_and_volume(bed, gain) {
        tracing::warn!("preview: {e}");
    }
    st.applied = generation;
    inner.applied.notify_all();
}

/// The background job: render (or reuse) the clip audio, mix, swap in.
fn mix(
    inner: &Inner,
    generation: u64,
    audio: &AudioTransport,
    music: Option<Arc<PcmAudio>>,
    volume: f32,
    key: ClipKey,
) {
    let cached = {
        let st = lock(&inner.state);
        if st.requested != generation {
            return;
        }
        st.clip
            .as_ref()
            .filter(|(k, _)| *k == key)
            .map(|(_, pcm)| Arc::clone(pcm))
    };
    let clip = match cached {
        Some(pcm) => Ok(pcm),
        None => {
            let opts = ClipAudioOptions {
                sample_rate: audio.sample_rate(),
                channels: audio.channels(),
                length: key.length,
            };
            render_clip_audio(&key.edl, &key.sources, &opts).map(Arc::new)
        }
    };
    let mixed = clip.and_then(|clip| {
        let music = PcmAudio {
            samples: music.as_ref().map_or_else(Vec::new, |m| {
                m.samples
                    .iter()
                    .map(|s| (s * volume).clamp(-1.0, 1.0))
                    .collect()
            }),
            sample_rate: clip.sample_rate,
            channels: clip.channels,
        };
        mix_under_music(&music, &clip).map(|m| (clip, m))
    });
    let mut st = lock(&inner.state);
    match mixed {
        Ok((clip, bed)) => {
            st.clip = Some((key, clip));
            apply(inner, &mut st, generation, audio, Some(Arc::new(bed)), 1.0);
        }
        Err(e) => {
            // Music alone beats silence.
            tracing::warn!("preview: cannot render clip audio: {e}");
            apply(inner, &mut st, generation, audio, music, volume);
        }
    }
}
