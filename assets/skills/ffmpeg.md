# FFmpeg assembly and verification

1. Run `skywrite doctor` and `skywrite inspect INPUT`.
2. Write an edit following `skywrite schema`, with local sources relative to the edit JSON. Source `duration` is output time; `speed` is source seconds per output second.
3. Run `skywrite edit validate edit.json` and review all reported rounding and bounds.
4. Render directly: `skywrite edit render edit.json --output edit.mp4`.
5. For a portable script, use `skywrite edit export edit.json --to ffmpeg --output render.py`, then `python3 render.py --output edit.mp4`.
6. Inspect the rendered MP4 and watch/listen across cuts, delays, gaps, held images, retiming, and additional audio. Confirm duration and streams against the approved edit.

Use explicit `--overwrite` only for an intended output replacement. The generated script needs Python's standard library and installed FFmpeg. Unsupported effects must be authored in a composition or NLE instead of being silently omitted.
