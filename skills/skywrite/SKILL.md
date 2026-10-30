---
name: skywrite
description: Understand local media, synchronize recordings with offset and drift, create validated assembly edits, and export or render them through the Rust Skywrite CLI.
---

Use installed `skywrite` (or the repository's `target/release/skywrite`). Start with `skywrite doctor` and `skywrite schema`. Successful commands return JSON. Progress/errors go to stderr and failures exit nonzero. Do not assume a vision model ID: use the user's `OPENROUTER_VISION_MODEL` or a current explicitly selected OpenRouter model.

1. Inspect sources: `skywrite inspect <files...>`. Understand durations, streams, frame rates, and audio availability.
2. Sample visuals without a paid call: `skywrite analyze video.mp4 --local --frames 12 -o analysis.json`. Use your image viewer on the returned JPEG paths. Sparse samples do not reveal every event or exact shot boundary.
3. Get words: `skywrite transcribe source.mp4 -o transcript.json`. This explicitly sends bounded audio to OpenRouter. Honor the user's API budget. Cached exact requests reuse results; do not repeatedly disable the cache.
4. Request optional structured visual analysis: `skywrite analyze source.mp4 --model MODEL --transcript transcript.json --prompt 'Editorial intent' -o understood.json`.
5. Sync each camera to the chosen master: `skywrite sync --reference master.transcript.json --asset camera.transcript.json`. For shared audio without transcribing use media paths and `--method audio`. Check quality, matched span, residuals, and whether drift was measurable. Do not use an ambiguous match automatically.
6. The time mapping is `reference_time = offset_seconds + scale*asset_time`. At reference time `r`, the asset source in point is `(r-offset_seconds)/scale`; its edit speed is `playback_rate=1/scale`. Trim to overlap. Do not confuse speed with duration scale.
7. Write the small edit JSON from `skywrite schema`, using real sources and complete thoughts. Primary clips are sequential; audio overlays have output offsets. Duration is OUTPUT seconds and source consumption is `duration*speed`. Null source creates black/silence. Stills have source_in 0 and speed 1.
8. `skywrite edit validate edit.json` before exporting/rendering. Read frame-rounding warnings. `skywrite edit suggest --analysis understood.json --brief 'Intent' --model MODEL -o edit.json` can draft a validated edit, but you still judge its quality.
9. Render a low-resolution assembly using the edit canvas and `skywrite edit render edit.json -o preview.mp4`; inspect frames/listen to audio around cuts. Iterate the edit JSON, not original media.
10. Deliver `skywrite edit export edit.json --to fcpxml -o edit.fcpxml`, `--to ffmpeg -o render.py`, or an optional `--to hyperframes|remotion -o NEW_PROJECT_DIRECTORY`. Optional composition runtimes are only required for those targets. You may extend generated projects with graphics/effects.

Use `skywrite image render --file title.html -o title.png --width 1280 --height 720` for native still graphics using embedded upstream Takumi 2.19.0 and the bundled OFL Geist font. Use inline HTML style/tw attributes or upstream JSON nodes; local image paths resolve relative to the template. Register extra font files with repeated `--font PATH`. No Takumi executable is required. WebP/JPEG support `--quality`; PNG is lossless. Remote resources, SVG, stylesheet blocks, and JavaScript are unsupported. Then add the still to the edit. The explicit `--backend takumi-cli` compatibility adapter requires a separately installed CLI.

Never overwrite an original media file. Existing outputs require `--overwrite`. Do not fabricate word timestamps or claim events outside the observed coverage. Model suggestions are candidate edits, not authoritative shot detection. Preserve source licenses/attribution in any shared fixture or deliverable. No ads capabilities are part of this CLI.
