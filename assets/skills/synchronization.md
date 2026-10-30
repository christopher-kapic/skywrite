# Align independent recordings

1. Inspect reference and asset files, preserving both originals. Choose a stable reference clock.
2. Run `skywrite sync --reference reference.wav --asset camera.mp4 --method auto`. Audio alignment is local; explicit transcription is a separate model command.
3. Review the confidence, ambiguity, anchors, drift, and analyzed-span diagnostics. Silence, repetitions, short overlap, and long unexamined tails need manual review; do not infer certainty from a returned offset alone.
4. The reported mapping is `reference_time = offset + scale * asset_time`. Place the asset on the reference timeline using that mapping; conform its playback with `speed = 1 / scale`.
5. If needed, produce transcripts explicitly with `skywrite transcribe INPUT` and consult `skywrite sync --help` for transcript input options and supported controls.
6. Write and validate the edit, render a review copy, and listen near the start, middle, and end for drift and double audio. Mute camera audio when using the independent recorder.

Source trims, timeline placement, and playback speed have different meanings. Never apply an offset as a source trim without deriving the desired overlap first.
