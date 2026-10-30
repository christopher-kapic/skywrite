# Remotion composition

1. Create and validate a version 1 edit using `skywrite schema` and `skywrite edit validate edit.json`.
2. Export to a new directory: `skywrite edit export edit.json --to remotion --output remotion-project`.
3. Review the generated composition and copied media. The current exporter accepts integer FPS and rounds source trims to composition frames; check those boundaries.
4. Install the optional Remotion runtime dependencies with `npm install` inside that project. Review dependencies and the Remotion license for your use.
5. Run `npm run studio` to author effects, then `npm run render` for the configured output.
6. Inspect and watch the result, checking audio sync and all cuts.

Remotion is optional; direct FFmpeg rendering and FCPXML export need no Node runtime. Preserve the exported source project for later edits.
