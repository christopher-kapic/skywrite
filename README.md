# Skywrite

A Rust CLI that gives coding agents a structured view of local media and a small edit format they can validate, render, or export. It uses FFmpeg/ffprobe directly. OpenRouter powers optional transcription, media understanding, and edit suggestions. Hyperframes, Remotion, and Takumi are optional output tools.

This is an assembly editor: source selection, in points, order, gaps, stills, constant speeds, and audio overlays. Finish detailed transitions, titles, grading, and effects in your NLE or exported composition project.

## Build

Rust 1.88 or newer, FFmpeg, and ffprobe are required. FFmpeg must include H.264 (`libx264`) and AAC encoding for the built-in MP4 renderer.

```sh
cargo build --release --locked
./target/release/skywrite doctor
```

The core is Rust. Direct dependencies are established projects: clap, serde/serde_json, anyhow, reqwest with Rustls, base64, RustCrypto sha2, tempfile, url, and libc. `Cargo.lock` records the exact resolved dependency tree. No Python or Node runtime is needed for inspection, synchronization, FCPXML export, or the native FFmpeg renderer.

All successful commands emit one JSON document on stdout. Progress and errors go to stderr; errors have a JSON `error` object and nonzero exit status. `--help` is human-readable. Output files are never replaced without `--overwrite`. Original media cannot be used as an output destination. Paths in an edit are relative to the edit JSON file; exported FCPXML uses absolute file URLs.

## Understand media

```sh
skywrite inspect camera-a.mp4 camera-b.mp4 recorder.wav
skywrite analyze camera-a.mp4 --local --frames 12 -o camera-a.local.json
```

Local analysis creates a manifest and persistent JPEG frames under `.skywrite/frames/`. An agent with image tools can inspect the listed frame paths directly. Frame times are source playback seconds. Uniform samples are sparse evidence: they do not establish exact shot boundaries or reveal every event in a long recording.

For optional OpenRouter calls, set `OPENROUTER_API_KEY` in the environment. Select a currently available vision model with `--model MODEL_ID` or `OPENROUTER_VISION_MODEL`; Skywrite does not guess an evolving vision model ID.

```sh
skywrite transcribe recorder.wav --language en -o recorder.transcript.json
skywrite analyze camera-a.mp4 --model YOUR_VISION_MODEL --prompt 'Find useful interview selects and cutaways' -o camera-a.analysis.json
skywrite analyze recorder.wav --model YOUR_AUDIO_CAPABLE_MODEL --transcript recorder.transcript.json -o recorder.analysis.json
```

Transcription uses OpenRouter's `/audio/transcriptions`, requests `verbose_json` with word timestamps, and sends bounded 16-kHz mono WAV chunks (60 seconds by default; adjustable from 10–120). Responses lacking actual word timestamps are rejected instead of estimating word positions. Chunk timestamps are shifted into source time, preserving any initial audio-stream delay. Generated transcripts include a source-content SHA-256 fingerprint; analysis rejects them if the source content changes. Manually supplied transcripts without fingerprints are accepted and marked unverified in the coverage report. Text can lose context at chunk boundaries; inspect important edits around those boundaries.

`--start SECONDS --duration SECONDS` limits the selected range for transcription and analysis. Transcription word positions remain relative to the full source, including that starting offset. A transcript supplied to `analyze --transcript` must reference the same source file. Audio-only remote analysis without a transcript sends at most the first 60 seconds of the chosen range and labels that coverage. Video analysis sends sampled frames; include a transcript to supply dialogue.

Explicit remote commands may incur OpenRouter charges. Requests have time, size, frame, and output-token limits, with no automatic paid retries. Exact API request content, model, endpoint, and API base are hashed into the `.skywrite/cache/` response cache; repeated identical requests reuse it. `--cache-dir PATH` relocates it and `--no-cache` disables it. The cache contains media-derived text; delete it when no longer needed. Cached requests can be used without a key. The CLI does not enforce an account-wide dollar budget; use a capped API key.

## Synchronize recordings

For speech, transcribe each recording and match timestamped words:

```sh
skywrite transcribe camera-a.mp4 -o camera-a.transcript.json
skywrite sync --reference recorder.transcript.json --asset camera-a.transcript.json -o camera-a.sync.json
```

Or synchronize common audio directly, without any API request:

```sh
skywrite sync --reference recorder.wav --asset camera-a.mp4 --method audio -o camera-a.sync.json
```

The result always maps **asset time into reference time**:

```text
reference_time = offset_seconds + scale * asset_time
playback_rate = 1 / scale
```

If `offset_seconds=12` and `scale=1.01`, asset zero belongs at reference second 12 and the asset needs a playback multiplier of approximately 0.990099. To find the asset in point corresponding to reference time `r`, use `(r-offset_seconds)/scale`. In edit JSON, `speed` is that playback multiplier. Trim to the actual overlap; a negative source in point is invalid.

Transcript matching uses word/phrase anchors and robust affine fitting to tolerate recognition errors and outliers. Audio alignment uses bounded signal analysis and local delay measurements. Results expose anchor coverage, residuals, ambiguity, and whether drift was estimable. A short or repetitive overlap cannot establish a reliable speed correction. Cuts, pauses, or dropped samples can require separate segments; a single affine result must not be treated as a universal time warp. Different microphone positions can introduce acoustic delays; synchronizing scratch audio does not independently certify camera lip sync.

Default audio input limit is one hour per file; `--max-audio-seconds` changes the bounded limit. Other options include `--max-scale-deviation`, `--tolerance-seconds`, `--min-anchors`, and `--min-span-seconds`. `--method auto` treats two `.json` inputs as transcripts and other inputs as media.

## Create an edit

`skywrite schema` prints agent-facing JSON Schema guidance and examples. A simple edit:

```json
{
  "version": 1,
  "name": "First assembly",
  "width": 1280,
  "height": 720,
  "fps": {"num": 30, "den": 1},
  "clips": [
    {"source": "camera-a.mp4", "source_in": 2, "duration": 5, "volume": 0},
    {"source": "title.png", "duration": 2},
    {"source": "camera-b.mp4", "source_in": 20, "duration": 3, "volume": 0}
  ],
  "audio": [
    {"source": "recorder.wav", "source_in": 12, "offset": 0, "duration": 10, "volume": 1}
  ]
}
```

Primary clips play sequentially. Omit `source` or set it to `null` for a black, silent gap. Additional audio clips use absolute output `offset` values. `source_in` and `duration` are seconds; **duration is output duration**, and a clip consumes `duration * speed` source seconds. `speed` defaults to 1 and `volume` is linear gain (0 mutes). Stills require `source_in=0` and `speed=1`. Primary sound-only media belongs in the `audio` array with a gap/still/video primary timeline.

Validation checks real source files, source ranges, finite positive numbers, supported rates, stream types, even dimensions, and track duration. Output durations and offsets are rounded to the rational frame grid, with warnings in the validation result. Use `fps={"num":30000,"den":1001}` for NTSC timing. Sources use their presentation timeline; changing a sample-rate label or frame-rate declaration does not itself prove clock drift.

An external agent can write the JSON itself, or request a model draft from saved analyses:

```sh
skywrite edit suggest --analysis camera-a.analysis.json recorder.analysis.json --brief 'Make a concise opening with complete thoughts' --model YOUR_TEXT_MODEL -o edit.json
skywrite edit validate edit.json
```

Suggestions use only analyzed sources and are validated before being written. Models still need editorial review; sparse observations cannot guarantee a good cut. The deterministic renderer never executes generated Python/JavaScript or asks a model to hand-author FCPXML.

## Export and render

```sh
skywrite edit export edit.json --to fcpxml -o edit.fcpxml
skywrite edit render edit.json -o preview.mp4
skywrite edit export edit.json --to ffmpeg -o render.py
python3 render.py preview.mp4
skywrite edit export edit.json --to hyperframes -o hyperframes-project
skywrite edit export edit.json --to remotion -o remotion-project
```

- **FCPXML:** generates an FCPXML 1.10 assembly with rational timing, original-media references, audio lanes, and constant retiming. Actual Final Cut/Resolve import must be checked in those applications; generating XML on Linux cannot certify application import fidelity.
- **Native render:** calls installed FFmpeg, fits video/stills into the output canvas, conforms constant speed, mixes audio, and creates an MP4. It preserves the originals. The generated Python renderer uses only the Python standard library and FFmpeg, with the same compiled edit operations.
- **Hyperframes:** exports a new HTML composition project and copied assets for the installed CLI. Follow the generated README for lint/preview/render. Existing project directories are never overwritten.
- **Remotion:** exports a new React composition project and copied assets. Install the generated npm dependencies inside that project, then preview/render using its scripts. This optional backend currently requires integer fps; use FCPXML/FFmpeg for rational fps. Remotion's own runtime and licensing apply.

Unsupported target behavior is reported rather than silently discarded. Exported projects are starting points an agent can extend with graphics and effects; the shared assembly format intentionally stays small.

## Still graphics with an external Takumi CLI (optional adapter)

```sh
skywrite image render --backend takumi-cli --file title.html -o title.png --width 1280 --height 720
```

This adapter explicitly calls whichever external `takumi` CLI is on PATH. In this workspace that is an older fork; this initial pass does not embed the current original upstream Takumi implementation. It uses direct arguments, checks the actual output dimensions, and publishes the image atomically. Supported formats are PNG, WebP, and JPEG, with `--quality 0..100` and repeated `--font GLOB` for local fonts. The format is inferred from the output extension unless supplied explicitly. Include the resulting image in the edit's primary clips. Takumi HTML/JSON and any referenced images/fonts follow Takumi's own supported input behavior.

## Validation

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

Integration tests require installed FFmpeg/ffprobe and exercise actual decoding, rendering, and known timing transformations. Paid network tests are explicit/manual: `transcribe` and `analyze` with a capped OpenRouter key and a small licensed fixture. Do not confuse local/cached protocol tests with a successful provider call.

The public AMI corpus (CC BY 4.0) is suitable for multi-microphone synchronization tests; controlled copies with known trims/rates provide ground truth. `https://groups.inf.ed.ac.uk/ami/download/` lists downloads and attribution. Synthetic fixtures are generated locally for deterministic regression checks. Review run evidence and unavailable checks are recorded separately from the application.
