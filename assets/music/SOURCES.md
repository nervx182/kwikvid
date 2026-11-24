# Bundled music: sources and licence evidence

Status: **shortlist, awaiting owner approval** (issue #48). No audio is
committed yet; bundling the approved files is the follow-up "Bundle the
approved CC0 tracks". `manifest.toml` next to this file is the draft
machine-readable list.

## Rules for a track to be eligible

- Licence is **CC0-1.0** exactly (public domain dedication). Not CC-BY, not
  "royalty free", not Pixabay or YouTube Audio Library licences (ADR 0005).
- Instrumental, suits action-cam edits, clear beat.
- The file shipped is the one downloaded **from the CC0 source page listed
  here**. Several picks are by Kevin MacLeod, who also publishes many titles on
  incompetech.com under **CC-BY 4.0**; a copy from incompetech, YouTube or a
  mirror is not covered by this record.
- Shipped as MP3 or AAC at <= 192 kbit/s. Re-encoding a CC0 file is allowed
  (CC0 waives all rights, including adaptation).

## Source: FreePD.com

All picks come from FreePD.com, which publishes its whole catalogue under
CC0. Evidence, as fetched on 2026-11-24 from
<https://freepd.com/electronic.php> (picks 1, 3, 4, alternate A) and
<https://freepd.com/upbeat.php>:

> Header: "100% Free Music - Creative Commons 0 No Attribution - Copyright Free*"
>
> Footer: "FreePD.com - 100% Free Music - Free for Commercial Use, Free Of
> Royalties, Free Of Attribution, Creative Commons 0"
>
> Footer: "* It is "copyright free" to the extent that the law allows."
>
> Each track entry: "This music is available for commercial and
> non-commercial purposes."

A third-party mirror of the FreePD catalogue
(<https://a.ocv.me/pub/stuff/music/freepd/README.md>) quotes FreePD as: "This
music is all licensed CC0 1.0 Universal Public Domain Dedication." It is
corroboration only, not a source to download from.

**Archived copy: pending.** web.archive.org was not reachable from the cloud
session that wrote this file. Before bundling, save
<https://freepd.com/electronic.php> with <https://web.archive.org/save> and
put the snapshot URL here.

## Shortlist

Duration, tempo, size and SHA-256 are **pending**: the cloud session that
wrote this could read the FreePD pages but its network policy refused
downloads from freepd.com (and from every other music host tried), so no file
could be measured. "Mirror size" is the size of the same file name on the
mirror above, for a rough budget only; it is not the shipped file. Genre
tempos are guesses until measured.

| # | Title | Artist | Source page | Licence | Role (variety) | Format | Mirror size |
|---|---|---|---|---|---|---|---|
| 1 | Arpent | Kevin MacLeod | <https://freepd.com/electronic.php> | CC0-1.0 | upbeat, ~120-130 BPM (guess) | MP3 | 6,486,643 B |
| 2 | 3 am West End | statusq | <https://freepd.com/electronic.php> | CC0-1.0 | calm, ~80-100 BPM (guess) | MP3 | not on mirror |
| 3 | Beat One | Kevin MacLeod | <https://freepd.com/electronic.php> | CC0-1.0 | high-energy, drum and bass ~170 BPM (guess) | MP3 | 5,835,821 B |
| 4 | Backbeat | Kevin MacLeod | <https://freepd.com/electronic.php> | CC0-1.0 | upbeat / punchy, short | MP3 | 1,922,353 B |

Alternates on the same page and licence, if a pick fails measurement or the
owner dislikes it: A. "Meditating Beat" (Kevin MacLeod, calm-ish, mirror
6,291,368 B); B. "Goodnightmare" (Kevin MacLeod, upbeat and driving, mirror
7,684,215 B); C. "Bit Bit Loop" (Kevin MacLeod, 8-bit party track, high
energy, mirror 3,076,298 B).

### Why each fits

1. **Arpent**: FreePD describes it as upbeat, uplifting and inspiring with a
   technology / innovation feel. Steady electronic pulse that the beat
   detector should lock onto; the default "happy montage" track.
2. **3 am West End**: relaxed, chill groove over dreamy synth and trumpet
   textures. The calm option for scenic footage (hiking, sunsets, slow-mo).
3. **Beat One**: upbeat drum and bass style, minimal, modern and dark. The
   high-energy option for MTB, surf, snow. Drum and bass may be detected at
   half tempo (~85 BPM); that is fine for cutting, but record what
   `analyze-music` reports.
4. **Backbeat**: upbeat, heavy-hitting beat with bass synth. Smallest file of
   the set (likely under a minute and a half); good for short 15-30 s edits.

### Per-track measurements (fill in before bundling)

| # | Duration | Tempo (`kwikvid analyze-music`) | Bitrate | Size (shipped file) | SHA-256 (shipped file) |
|---|---|---|---|---|---|
| 1 | pending | pending | pending | pending | pending |
| 2 | pending | pending | pending | pending | pending |
| 3 | pending | pending | pending | pending | pending |
| 4 | pending | pending | pending | pending | pending |

How to fill it (any machine that can reach freepd.com; play each title on
the source page and use its download link, the URL is usually
`https://freepd.com/music/<Title>.mp3` (guess, verify)):

```sh
ffprobe -v error -show_entries format=duration,bit_rate -of default=nw=1 "Arpent.mp3"
# if bit_rate > 192000, re-encode and measure the re-encoded file instead:
ffmpeg -i "Arpent.mp3" -codec:a libmp3lame -b:a 192k arpent.mp3
cargo run -p kv-cli --release -- analyze-music arpent.mp3
stat -c %s arpent.mp3        # macOS: stat -f %z
sha256sum arpent.mp3         # macOS: shasum -a 256
```

Note: an FFmpeg build with libmp3lame is fine for this one-off conversion; it
is a tool run on the file, not a dependency of kwikvid.

## Size budget and storage

Mirror sizes of picks 1, 3 and 4 add up to about 14.2 MB, plus pick 2
(unknown, guess 5-7 MB): about 20 MB as published. FreePD MP3s are commonly
higher than 192 kbit/s (guess); re-encoded to 192 kbit/s the set should land
around 12 MB, under the 15 MB budget (guess, confirm with the table above).
If it does not, drop Backbeat or use 160 kbit/s.

Every pick will be over 1 MB, so per the issue's (reversible) rule the bundled
files go in **Git LFS** under `assets/music/`, like the regression corpus
(ADR 0008). The bundling follow-up adds
`assets/music/*.mp3 filter=lfs diff=lfs merge=lfs -text` to `.gitattributes`.
The repo is public now, so LFS bandwidth for public clones counts against the
owner's quota; if that becomes a problem, fetching the files at build time
from a release asset is the alternative.

## Not used, and why

- incompetech.com (Kevin MacLeod): CC-BY 4.0, needs attribution.
- ccMixter: mostly CC-BY / CC-BY-NC.
- Pixabay Music, YouTube Audio Library, Uppbeat: own licences, not CC0.
- Free Music Archive and OpenGameArt: have CC0 tracks, but their pages could
  not be read from the cloud session; worth a look if more choice is wanted.
- Anything from GoPro Quik: proprietary (ADR 0005).
