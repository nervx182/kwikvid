//! The beat grid: the timeline points where cuts may land, and the music
//! energy that sets how long each slot between cuts should be.

use kv_audio::{MusicAnalysis, BEATS_PER_BAR, ENERGY_BLOCK};

use crate::{AutoEditError, MAX_SLOT_BEATS};

/// Downbeats count as matching a beat within this many microseconds.
const MATCH_TOLERANCE: i64 = 1_000;
/// Slot length for loud, average and quiet passages, in microseconds.
const SLOT_LOUD: i64 = 1_000_000;
const SLOT_AVERAGE: i64 = 2_000_000;
const SLOT_QUIET: i64 = 3_000_000;
/// Window over which the local music energy is measured.
const ENERGY_WINDOW: i64 = 1_000_000;

pub(crate) struct Grid {
    /// `points[0]` is the timeline start (0), then every beat, increasing.
    pub points: Vec<i64>,
    /// Whether each point is a downbeat (`points[0]` counts as one).
    pub downbeat: Vec<bool>,
    /// Index of the point where the edit ends (the beat closest to the
    /// target length).
    pub n: usize,
    /// Median beat interval.
    pub median_iv: i64,
    energy: Vec<f32>,
    mean_energy: f32,
}

impl Grid {
    pub(crate) fn build(music: &MusicAnalysis, target: i64) -> Result<Grid, AutoEditError> {
        let mut beats: Vec<i64> = music.beats.iter().map(|b| b.0).filter(|&b| b > 0).collect();
        beats.sort_unstable();
        beats.dedup();
        if beats.len() < 2 {
            return Err(AutoEditError::NotEnoughBeats);
        }
        let mut ivs: Vec<i64> = beats.windows(2).map(|w| w[1] - w[0]).collect();
        ivs.sort_unstable();
        let median_iv = ivs[ivs.len() / 2];

        let mut downbeats: Vec<i64> = music.downbeats.iter().map(|d| d.0).collect();
        downbeats.sort_unstable();
        let mut flags: Vec<bool> = beats
            .iter()
            .map(|&b| {
                let i = downbeats.partition_point(|&d| d < b - MATCH_TOLERANCE);
                downbeats
                    .get(i)
                    .is_some_and(|&d| (d - b).abs() <= MATCH_TOLERANCE)
            })
            .collect();
        if !flags.contains(&true) {
            flags = (0..beats.len()).map(|i| i % BEATS_PER_BAR == 0).collect();
        }

        // A beat right after the start would make a uselessly short first slot.
        while beats.len() > 1 && beats[0] < median_iv / 2 {
            beats.remove(0);
            flags.remove(0);
        }

        // Music shorter than the target: continue the grid at the median tempo.
        let last_db = flags.iter().rposition(|&f| f).unwrap_or(0);
        let limit = target.saturating_add(median_iv);
        while beats[beats.len() - 1] < limit {
            let i = beats.len();
            beats.push(beats[i - 1] + median_iv);
            flags.push((i - last_db) % BEATS_PER_BAR == 0);
        }

        let mut points = Vec::with_capacity(beats.len() + 1);
        points.push(0);
        points.extend_from_slice(&beats);
        let mut downbeat = Vec::with_capacity(points.len());
        downbeat.push(true);
        downbeat.extend_from_slice(&flags);

        let n = (1..points.len())
            .min_by_key(|&i| (points[i] - target).abs())
            .unwrap_or(1);

        let end_block = (points[n] / ENERGY_BLOCK.0) as usize;
        let used = &music.energy[..end_block.min(music.energy.len())];
        let mean_energy = if used.is_empty() {
            0.0
        } else {
            used.iter().sum::<f32>() / used.len() as f32
        };

        Ok(Grid {
            points,
            downbeat,
            n,
            median_iv,
            energy: music.energy.clone(),
            mean_energy,
        })
    }

    /// Timeline length of `k` beats starting at point `c`.
    pub(crate) fn dur(&self, c: usize, k: usize) -> i64 {
        self.points[c + k] - self.points[c]
    }

    /// Spacing of evenly spread candidate windows, and their nominal length.
    pub(crate) fn nominal(&self) -> i64 {
        (4 * self.median_iv).max(3_500_000)
    }

    /// Music energy over the next second relative to the edit's average.
    fn relative_energy(&self, t: i64) -> f32 {
        let from = (t / ENERGY_BLOCK.0) as usize;
        let to = ((t + ENERGY_WINDOW) / ENERGY_BLOCK.0) as usize;
        let blocks = &self.energy[from.min(self.energy.len())..to.min(self.energy.len())];
        if blocks.is_empty() || self.mean_energy <= 0.0 {
            return 1.0;
        }
        blocks.iter().sum::<f32>() / blocks.len() as f32 / self.mean_energy
    }

    /// [`Self::slot_beats`] moved to a downbeat one beat away, if there is
    /// one: the length a free slot at `c` usually ends up with.
    pub(crate) fn snapped_slot_beats(&self, c: usize) -> usize {
        let k = self.slot_beats(c);
        let ok = |k: usize| k > 0 && self.downbeat.get(c + k).copied().unwrap_or(false);
        if ok(k) {
            k
        } else if ok(k + 1) {
            k + 1
        } else if ok(k - 1) {
            k - 1
        } else {
            k
        }
    }

    /// Desired slot length in beats for a slot starting at point `c`: shorter
    /// where the music is louder.
    pub(crate) fn slot_beats(&self, c: usize) -> usize {
        let rel = self.relative_energy(self.points[c]);
        let want = if rel > 1.15 {
            SLOT_LOUD
        } else if rel < 0.85 {
            SLOT_QUIET
        } else {
            SLOT_AVERAGE
        };
        // The first point is the timeline start, not a beat: use the
        // interval of the first whole beat there.
        let iv = match c {
            0 if self.n > 1 => self.dur(1, 1),
            c if c < self.n && c > 0 => self.dur(c, 1),
            _ => self.median_iv,
        };
        ((want as f64 / iv as f64).round() as usize).clamp(1, MAX_SLOT_BEATS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kv_core::Micros;

    fn music(beats: &[i64], energy: Vec<f32>) -> MusicAnalysis {
        MusicAnalysis {
            duration: Micros(beats.last().copied().unwrap_or(0)),
            tempo_bpm: 120.0,
            beats: beats.iter().map(|&b| Micros(b)).collect(),
            downbeats: beats.iter().step_by(4).map(|&b| Micros(b)).collect(),
            onsets: Vec::new(),
            energy,
        }
    }

    #[test]
    fn extends_past_the_music_and_picks_closest_end() {
        let beats: Vec<i64> = (1..=8).map(|i| i * 500_000).collect();
        let g = Grid::build(&music(&beats, vec![0.5; 40]), 10_100_000).unwrap();
        assert_eq!(g.points[g.n], 10_000_000);
        assert_eq!(g.median_iv, 500_000);
        // Downbeats continue every 4 beats past the music (4.5 s, 6.5 s, ...).
        assert!(g.downbeat[1] && g.downbeat[5] && g.downbeat[9] && g.downbeat[13]);
        assert!(!g.downbeat[10]);
    }

    #[test]
    fn drops_a_beat_right_at_the_start() {
        let mut beats: Vec<i64> = (1..=8).map(|i| i * 500_000).collect();
        beats.insert(0, 10_000);
        let g = Grid::build(&music(&beats, vec![]), 3_000_000).unwrap();
        assert_eq!(g.points[1], 500_000);
    }

    #[test]
    fn louder_music_gives_shorter_slots() {
        let beats: Vec<i64> = (1..=40).map(|i| i * 500_000).collect();
        let mut energy = vec![0.1; 100];
        energy.extend(vec![0.5; 100]);
        let g = Grid::build(&music(&beats, energy), 20_000_000).unwrap();
        assert_eq!(g.slot_beats(2), 6); // quiet: 3 s at 0.5 s per beat
        assert_eq!(g.slot_beats(30), 2); // loud: 1 s
    }

    #[test]
    fn too_few_beats() {
        assert!(matches!(
            Grid::build(&music(&[500_000], vec![]), 1_000_000),
            Err(AutoEditError::NotEnoughBeats)
        ));
    }
}
