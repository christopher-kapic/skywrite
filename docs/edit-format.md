# Edit format, version 2

Skywrite compiles a small layered JSON edit into an MP4 render, an executable
Python/FFmpeg script, FCPXML, Premiere-importable XML, or an optional
Hyperframes/Remotion project. Agents write this edit format rather than
authoring interchange XML.

```json
{
  "version": 2,
  "name": "Interview",
  "width": 1920,
  "height": 1080,
  "fps": {"num": 30, "den": 1},
  "sources": [
    {"path": "camera.mxf", "lut": "luts/clog2-to-709.cube",
     "color": {"transfer": "canon-log2", "primaries": "cinema-gamut", "note": "confirmed by operator"}}
  ],
  "video": [
    {"name": "A-roll", "clips": [
      {"start": 0, "source": "camera.mxf", "source_in": 12.4, "duration": 9, "audio": {"volume": 0}},
      {"start": 9, "source": "title.png", "duration": 3, "fade_out": 1}
    ]},
    {"name": "B-roll", "clips": [
      {"start": 4, "source": "crowd.mp4", "source_in": 30, "duration": 3, "fade_in": 0.5, "audio": {"volume": 0}}
    ]}
  ],
  "audio": [
    {"name": "Dialogue", "clips": [
      {"start": 0, "source": "recorder.wav", "source_in": 20.1, "duration": 9, "speed": 0.9999, "fade_out": 0.25}
    ]},
    {"name": "Music", "clips": [
      {"start": 0, "source": "bed.mp3", "duration": 12, "volume": 0.5,
       "volume_keyframes": [{"time": 0, "volume": 1}, {"time": 1, "volume": 0.3},
                            {"time": 8.5, "volume": 0.3}, {"time": 9.5, "volume": 1}],
       "fade_out": 2}
    ]}
  ],
  "master": {"limiter_db": -1}
}
```

`skywrite schema` prints the complete JSON Schema with this example.

## Timeline model

Every clip has an explicit `start` on the output timeline. `video` is a list of
tracks; track 0 is the bottom layer and later tracks composite above it. Clips on
one track must not overlap; put overlapping material on another track. Empty
timeline regions are black and silent. The timeline ends at the latest clip end,
or at an explicit top-level `duration` (which may add trailing black).

Paths are local files, resolved relative to the edit JSON. Input files remain
unchanged. Sound-only media belong on `audio` tracks. A video clip's own sound is
*linked* to it and controlled by its `audio` object; set `"audio": {"volume": 0}`
to mute it, for example on B-roll above continuous dialogue or with a separate
recorder.

`source_in` is measured from the source's first presented sample (the container's
reported presentation start). Inspection reports elapsed presentation duration:
Matroska/WebM's ending timestamp is rebased to that start; MP4 and MPEG-TS
already report elapsed duration. `duration` is always **output seconds**;
`speed` is **source seconds per output second**. A 10-second output clip at speed
1.01 consumes 10.1 source seconds. If synchronization reports
`reference_time = offset + scale * asset_time`, use `speed = 1 / scale` to
conform that asset to the reference. Source trims are not timeline positions.

`start`, `duration` and the optional timeline `duration` are rounded to the
nearest output frame, and the validator reports rounding. `source_in` keeps its
precision (word-accurate dialogue cuts). Because rounding is to the nearest
frame, a clip that uses a source to its very end may run up to half an output
frame past it; that tail is accepted with a warning and padded (silence, held
last picture), so durations need not be floored. Longer overruns are rejected. Use rational rates such as
`{"num":30000,"den":1001}` for NTSC.

## Pictures

Video and stills scale to fit the canvas, preserving aspect ratio. Letterboxing is
transparent, so lower tracks show through; at the bottom that means black. Stills
keep their alpha channel (for example Takumi title cards with transparency) and
require `source_in: 0` and `speed: 1`.

- `opacity` (0–1, default 1) blends the picture over lower tracks.
- `fade_in` / `fade_out` (seconds) fade the picture from/to transparent; on the
  bottom track that is a fade from/to black.

## Sound

Audio clips and a video clip's `audio` object share the same controls:

- `volume`: linear gain, 0–4. 0 mutes; 1 is the original level.
- `volume_keyframes`: `[{"time": seconds_from_clip_start, "volume": gain}]`,
  strictly increasing, interpolated linearly and held before the first and after
  the last point. Use them to ride a music bed around dialogue and applause.
- `fade_in` / `fade_out`: linear fades in seconds.

Effective gain is `volume × keyframe envelope × fades`. All sound is mixed at
48 kHz stereo; a mono source is placed in the center at about -3 dB per channel.
Mixing sums tracks at unity gain. Nothing limits the mix unless you set
`master.limiter_db` (−24 to 0 dBFS); without it, loud overlaps can clip. Use
`skywrite verify` on renders to check peaks.

### Track roles

Any track may declare what its sound is: `"role": "dialogue"`, `"music"` or
`"effects"` (on a video track, the role describes its linked sound):

```json
"video": [{"name": "Interview", "role": "dialogue", "clips": [...]}],
"audio": [{"name": "Bed", "role": "music", "clips": [...]}]
```

Roles are optional and change nothing in a normal render. They enable:

- `skywrite edit render edit.json --roles dialogue -o check.mp4`, a stem render with
  only those roles' sound (pictures unchanged). Use it to verify dialogue by
  transcription without a music bed masking quiet words. Roles combine with commas
  (`--roles music,effects`). Every audible track must then declare a role; Skywrite
  refuses rather than guessing.
- FCPXML exports label clips with the matching audio role (an undeclared audio
  track stays FCP's default, dialogue). Premiere XML has no audio roles.
- `review lint` treats `music` and `effects` tracks as beds and `dialogue` tracks as
  dialogue, instead of inferring that from which sources have transcripts.

## Color

`sources` declares per-source settings. `color` records the operator-confirmed
capture encoding (for example Canon Log 2 / Cinema Gamut); it overrides container
metadata in reports but does not change pixels. `lut` names a `.cube` file
(1D or 3D, at most 32 MiB combined) that the MP4 render applies to that source
before scaling. Skywrite does not ship camera LUTs: use the manufacturer's or your
colorist's. A source declared as a log encoding without a LUT produces a warning,
because the render will look flat. Final grading belongs in the NLE.

## Importing from an NLE

```sh
skywrite edit import premiere-export.xml --output edit.json
skywrite edit import project.prproj --output edit.json   # experimental
```

Final Cut Pro 7 XML (what Premiere writes with File > Export > Final Cut Pro
XML) is the supported import: video tracks become layers, linked sound becomes
each clip's `audio`, unlinked audio becomes audio tracks (stereo pairs are
merged), and Time Remap speed, Opacity and Audio Levels values and keyframes,
fades, markers (as chapters) and the sequence rate and size carry over.
Generators, transitions and other effects are reported rather than silently
dropped. Media paths are kept absolute; if they live on another machine the
result reports validation errors so you can relink.

`.prproj` import is experimental and read-only: Premiere projects are
undocumented, gzip-compressed object graphs that change between versions. It
reads clip placement, in points and media paths only. Prefer exporting XML from
Premiere, and report mismatches with `skywrite nit`. Skywrite never writes
`.prproj` files.

## Version 1 edits

Version 1 (sequential `clips` plus `audio` overlays with `offset`) is no longer
accepted. Convert files without probing media:

```sh
skywrite edit migrate old-edit.json --output edit.json
```

Primary clips land on video track V1 at the frame where they used to start, gaps
become empty timeline (a trailing gap becomes an explicit `duration`), and
overlays are packed onto as few audio tracks as possible without overlap.

## Outputs

```sh
skywrite edit validate edit.json
skywrite edit render edit.json --output edit.mp4
skywrite edit export edit.json --to premiere --output edit.xml
skywrite edit export edit.json --to fcpxml --output edit.fcpxml
skywrite edit export edit.json --to ffmpeg --output render.py
python3 render.py --output edit.mp4
skywrite edit export edit.json --to hyperframes --output hyperframes-project
skywrite edit export edit.json --to remotion --output remotion-project
```

What each target carries:

| Feature | MP4 / Python | Premiere XML | FCPXML | Remotion | Hyperframes |
|---|---|---|---|---|---|
| Layers, timing, speed | yes | yes | yes | yes | yes |
| Opacity, picture fades | yes | Opacity keyframes | adjust-blend | yes | yes (GSAP) |
| Volume, keyframes, audio fades | yes | Audio Levels keyframes | adjust-volume | yes | constant volume only |
| LUT | yes | warning | warning | warning | warning |
| Master limiter | yes | warning | warning | warning | warning |
| Subframe `source_in` | yes | rounded to frames (warned) | yes | rounded to frames | yes |
| Captions / chapters | yes | markers only | note + markers | sidecars | sidecars |

The Python script uses only the standard library and FFmpeg; it invokes FFmpeg
with an argument list, never a shell. Both direct rendering and the script
publish an H.264/AAC MP4. LUT and caption text travel as private generated
sidecar files; only generated ASCII paths enter FFmpeg filters. On Linux, the
script stops its owned FFmpeg process group on ordinary SIGINT/SIGTERM
cancellation, timeout, or failure before removing its private temporary output.
The leader is observed with `waitid(WNOWAIT)` and remains unreaped until the final
group signal, preserving its process-group identity. Other POSIX platforms must
provide these wait primitives or the script refuses to start FFmpeg; their runtime
behavior has not been tested here. Existing output files require `--overwrite`.
Input edit, LUT, caption, transcript and analysis files and original media are
protected even with that flag; an exported Python renderer also protects its own
script. Exported project directories must be new even when `--overwrite` is
supplied, preserving authored projects.

**Premiere XML** (`--to premiere`) is Final Cut Pro 7 XML (`xmeml` version 5),
which Premiere Pro imports via File → Import. Each video track keeps its layer,
its linked sound goes to a matching audio track, and audio tracks follow. Speed
uses the Time Remap effect, opacity and fades use Opacity keyframes, and gain uses
Audio Levels keyframes. The format counts whole frames at integer or NTSC rates,
so subframe source in points are rounded (with a warning). Skywrite checks the
XML's structure and timing with an independent reader, but import into Premiere
itself has not yet been verified: please report results with `skywrite nit`.

**FCPXML 1.10** uses file URLs and rational time values. A full-length gap anchors
each track as a connected storyline (video tracks above, audio tracks below), so
no track's placement depends on another's retiming. It references original files
rather than embedding them. The format follows Apple's
[FCPXML reference](https://developer.apple.com/documentation/professional-video-applications/fcpxml-reference)
and [timeMap specification](https://developer.apple.com/documentation/professional-video-applications/timemap).
Actual import into Final Cut Pro requires validation on macOS.

Composition projects copy unique media into their output directory, with a
20 GiB total media limit. They need their optional runtime: installed Hyperframes,
or `npm install` inside the generated Remotion project followed by `npm run
studio`/`npm run render`. Hyperframes uses a pinned GSAP script; its renderer may
need network access or its own vendor cache. Remotion's runtime has its own
[license](https://www.remotion.dev/license). Neither runtime is needed for the
Rust CLI, FFmpeg rendering, or interchange export.

Limits: 16 video tracks, 32 audio tracks, 1024 video clips, 1024 audio clips,
256 volume keyframes per clip, 256 source settings, 24 hours of output, even
dimensions from 2 to 8192 pixels, 1–240 FPS, speed 0.125–8, and linear gain 0–4.
Missing files, absent media streams, overlaps within a track, and source ranges
exceeding the known duration are errors.

## Optional captions and chapters

Add `captions` and `chapters` when needed; both use the **output timeline**,
after cuts and retiming.

The simplest caption track is generated from word transcripts of the source
media and follows the edit automatically:

```json
"captions": [{"from_transcripts": ["interview.transcript.json"], "language": "eng"}]
```

Each word is captioned where its midpoint is audible in a clip of its source
(muted, cut and faded-to-silence words are skipped), at its output-timeline time;
cues break at cuts, and the same words heard from two synced sources are
captioned once. Write the track for an NLE with
`skywrite edit captions edit.json -o captions.srt`. Premiere exports write it
beside the XML automatically (`NAME.eng.srt`); FCPXML exports embed native
captions. SRT is the recommended deliverable because it stays editable as text.

A track can instead name an existing `source` file already on the output
timeline:

```json
{
  "version": 2,
  "duration": 10,
  "captions": [{"source": "captions.srt", "language": "eng"}],
  "chapters": [{"start": 0, "title": "Opening"}, {"start": 5, "title": "Next"}]
}
```

Caption paths resolve relative to the edit and accept UTF-8 `.srt`, plain `.vtt`,
or existing Skywrite transcript `.json` with real word timestamps. No ASR runs.
The track mapping is `output_time = offset + scale * caption_time`; `offset`
defaults to 0 and `scale` to 1. This is explicit mapping, not automatic caption
reassembly across source cuts. Prepare separate output-timeline cues when cutting
spoken source material. `language` is a three-letter lowercase ISO-639 code,
default `und`. `mode` defaults to `soft`, producing optional MP4 `mov_text`
subtitle tracks. Set `mode` to `burn` explicitly to render text into pixels;
this requires FFmpeg's `subtitles` filter with libass. Caption text is plain text;
markup is escaped and rendered literally through an internal WebVTT sidecar.
Braces and backslashes in caption text are rejected before export/rendering,
because FFmpeg cannot preserve them identically across soft tracks and burn-in.
Unicode, quotes, and ordinary newlines remain supported; chapter titles do not
have this subtitle-specific restriction. Plain SRT/VTT excludes styling, cue settings, and WebVTT regions.

Chapter starts must increase strictly and fall inside the output duration; each
chapter ends at the next start or the output end. Caption cues need ordered,
positive timestamps fitting the timeline. Times must remain distinct at
millisecond precision. Limits: 8 tracks, 100,000 total cues, 4 MiB text per track,
4096 bytes per cue, 8 MiB per caption input, 1000 chapters, 512 bytes per chapter
title; chapter/caption text excludes control characters. Remapping scales range
from 0.125 to 8; offsets are limited to ±24 hours. Original edit, caption,
transcript, and source media inputs are protected even with `--overwrite`.

FFmpeg native rendering and generated Python scripts apply captions and chapters.
FCPXML embeds caption tracks as native `<caption>` elements (SRT role) and
emits chapter markers; Final Cut/Resolve import of the captions has not been
verified. Premiere XML cannot carry captions: each track is written beside the
XML as SRT, to import together with it. Hyperframes and Remotion preserve SRT/VTT, chapter metadata,
and JSON sidecars; their generated compositions do not apply them automatically.
The FFmetadata sidecar contains chapter boundaries; JSON preserves exact titles,
including trailing backslashes that FFmetadata cannot round-trip reliably.
After rendering either optional backend, run:

```sh
skywrite captions mux rendered.mp4 --edit edit.json --output finished.mp4
```

This finishes a video matching the edit duration, copies its audio/video when
possible, adds soft tracks/chapters, and re-encodes video for explicit burn-in.
The original renderer output remains an input and cannot be replaced in place.
