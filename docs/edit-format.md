# Edit format, version 1

Skywrite compiles a small JSON edit into FCPXML, an executable Python/FFmpeg
script, a direct MP4 render, or an optional Hyperframes/Remotion project. Agents
write this edit format rather than authoring interchange XML.

```json
{
  "version": 1,
  "name": "Interview assembly",
  "width": 1920,
  "height": 1080,
  "fps": {"num": 30, "den": 1},
  "clips": [
    {"source": "camera.mp4", "source_in": 12.5, "duration": 5, "volume": 0},
    {"duration": 0.5},
    {"source": "title.png", "duration": 2},
    {"source": "camera.mp4", "source_in": 30, "duration": 8, "speed": 1.01}
  ],
  "audio": [
    {"source": "recorder.wav", "source_in": 20, "offset": 0, "duration": 5, "speed": 0.9999, "volume": 1}
  ]
}
```

Paths are local files, resolved relative to the edit JSON. Input files remain
unchanged. `clips` play sequentially; omit `source` or use `null` for black/silent
gaps. Sound-only media belong in `audio`, whose clips start at `offset` on the
output timeline and mix with camera audio. Set camera `volume` to zero when
using a separate recorder. Volumes are linear gains: zero is mute, one is the
original gain. Images are held for the requested duration, preserving their
aspect ratio with a black background.

`source_in` is measured from the source's first presented sample (the container's
reported presentation start). Inspection reports elapsed presentation duration:
Matroska/WebM's ending timestamp is rebased to that start; MP4 and MPEG-TS
already report elapsed duration. `duration` is
always **output seconds**; `speed` is **source seconds per output second**.
For example, a 10-second output clip at speed 1.01 consumes 10.1 source seconds.
If synchronization reports `reference_time = offset + scale * asset_time`, use
`speed = 1 / scale` to conform that asset to the reference. Source trims are not
the same as timeline offsets.

Durations and audio offsets are rounded to the nearest output frame, and the
validator reports rounding. Use rational rates such as `{"num":30000,"den":1001}`
for NTSC. FCPXML and FFmpeg retain rational timing; this initial Remotion exporter
accepts integer FPS and rounds source trims to composition-frame boundaries.
Audio/visual stream start offsets are preserved when FFmpeg renders selected
source ranges, including black video before a delayed video stream begins.
Playback-rate corrections preserve audio pitch. MPEG-TS sampling and source
trims decode from the beginning because transport streams may lack a reliable
seek index; inspection still reads metadata without decoding the source.

The initial format supports assembly cuts, gaps, held images, constant speed,
source audio gain, and additional audio layers. Unknown fields and unsupported
effects are rejected. Color grading, transitions, animated titles, and video
overlays can be added by an agent in an exported composition or finished in an
NLE. Takumi-generated images work as ordinary image clips.

## Outputs

```sh
skywrite edit validate edit.json
skywrite edit render edit.json --output edit.mp4
skywrite edit export edit.json --to fcpxml --output edit.fcpxml
skywrite edit export edit.json --to ffmpeg --output render.py
python3 render.py --output edit.mp4
skywrite edit export edit.json --to hyperframes --output hyperframes-project
hyperframes lint hyperframes-project
hyperframes render hyperframes-project --output edit.mp4
skywrite edit export edit.json --to remotion --output remotion-project
```

The Python script uses only the standard library and FFmpeg; it invokes FFmpeg
with an argument list, never a shell. Both direct rendering and the script
publish an H.264/AAC MP4. On POSIX, the script stops its owned FFmpeg process group on
ordinary SIGINT/SIGTERM cancellation, timeout, or failure before removing its
private temporary output. Existing output files require `--overwrite`. Rendering
or exporting over an original source is rejected. Exported project directories
must be new even when `--overwrite` is supplied, preserving authored projects.

FCPXML 1.10 uses file URLs and rational time values. A full-length primary gap
anchors a connected video storyline and separate audio lanes; this avoids making
external audio placement depend on the first shot's retiming. It references
original files rather than embedding them. The format follows Apple's
[FCPXML reference](https://developer.apple.com/documentation/professional-video-applications/fcpxml-reference)
and [timeMap specification](https://developer.apple.com/documentation/professional-video-applications/timemap).
Actual import into Final Cut Pro requires validation on macOS.

Composition projects copy unique media into their output directory, with a
20 GiB total media limit. They need their optional runtime: installed Hyperframes,
or `npm install` inside the generated Remotion project followed by `npm run
studio`/`npm run render`. Hyperframes uses a pinned GSAP script; its renderer may
need network access or its own vendor cache. Remotion's runtime has its own
[license](https://www.remotion.dev/license). Neither runtime is needed for the
Rust CLI, FFmpeg rendering, or FCPXML export.

Limits are explicit: 512 primary clips, 256 additional audio clips, 24 hours of
output, even dimensions from 2 to 8192 pixels, 1–240 FPS, speed 0.125–8, and
linear gain 0–4. Additional audio must fit within the primary timeline. Missing
files, absent media streams, and source ranges exceeding the known duration
are errors.
