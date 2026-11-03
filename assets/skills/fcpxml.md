# Final Cut Pro interchange

1. Inspect original media with `skywrite inspect INPUT` and prepare a version 2 edit from `skywrite schema`.
2. Validate with `skywrite edit validate edit.json`.
3. Export: `skywrite edit export edit.json --to fcpxml --output edit.fcpxml`.
4. Keep original media accessible: FCPXML references file URLs and does not embed files.
5. Import in Final Cut Pro on macOS, inspect each track as a connected storyline (video above, audio below), and verify source trims, retiming, opacity and volume keyframes, rational frame rate, and total duration. LUTs and the master limiter are not exported; apply them in the NLE.
6. Finish unsupported effects and titles in the NLE, then review the final export.

The exporter writes FCPXML 1.10. Linux-side validation cannot replace a real Final Cut import. Use a new output unless intentionally supplying `--overwrite`; originals remain protected.
