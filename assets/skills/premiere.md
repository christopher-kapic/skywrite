# Premiere Pro interchange

1. Inspect original media with `skywrite inspect INPUT` and prepare a version 2 edit from `skywrite schema`. Put continuous dialogue on the bottom video track or an audio track and B-roll/titles on upper video tracks.
2. Validate with `skywrite edit validate edit.json`.
3. Export: `skywrite edit export edit.json --to premiere --output edit.xml`. This writes Final Cut Pro 7 XML (xmeml v5), which Premiere imports with File → Import.
4. Read the JSON warnings. Subframe word-precise in points are rounded to whole frames; LUTs and the master limiter are not representable, so apply them in Premiere (Lumetri → Input LUT; a limiter on the mix bus) to match the rendered MP4.
5. Keep original media at the same absolute paths: the XML references `file://localhost/...` URLs and does not embed files.
6. After import, check layers, linked audio, speed (Time Remap), opacity fades, audio level keyframes, and total duration against a Skywrite render of the same edit.
7. Captions: add `{"from_transcripts": ["words.json"], "language": "eng"}` to the edit's `captions`. The export writes `edit.eng.srt` beside `edit.xml`; tell the user to import both files together in Premiere and drag the caption item onto the sequence at 00:00:00:00. Captions stay editable as text. Premiere XML itself cannot carry captions.
8. Bring an editor's changes back with `skywrite edit import edit.xml -o edit.json` (Premiere: File > Export > Final Cut Pro XML). `.prproj` import is experimental.
9. Record the result with `skywrite nit`, including the Premiere version, import errors, and any timing or level differences. Cite the exporter source (`src/export/xmeml.rs` at https://github.com/christopher-kapic/skywrite) when describing a mismatch.

Linux-side validation (an independent OpenTimelineIO reader) checks structure and timing, not Premiere's own import behavior.
