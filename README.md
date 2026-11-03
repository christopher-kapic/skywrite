# Skywrite

A Rust CLI that gives coding agents a structured view of local media and a small edit format they can validate, render, or export. It uses FFmpeg/ffprobe directly. OpenRouter powers optional transcription, media understanding, edit suggestions, and image/video/narration/music generation. Hyperframes and Remotion are optional output tools. Upstream Takumi is embedded for native still graphics.

This is an assembly editor: source selection, in points, order, gaps, stills, constant speeds, and audio overlays. Finish detailed transitions, titles, grading, and effects in your NLE or exported composition project.

## Build

Rust 1.91 or newer, FFmpeg, and ffprobe are required. FFmpeg must include H.264 (`libx264`) and AAC encoding for the built-in MP4 renderer.

```sh
cargo build --release --locked
./target/release/skywrite doctor
```

The core is Rust. Direct dependencies include established projects: clap, serde/serde_json, anyhow, reqwest with Rustls, RustCrypto sha2, tempfile, signal-hook, Tokio, and Apache Arrow’s `object_store` S3 backend. `Cargo.lock` records the exact resolved dependency tree. No Python or Node runtime is needed for inspection, synchronization, FCPXML export, or the native FFmpeg renderer.

All successful commands emit one JSON document on stdout. Progress and errors go to stderr; errors have a JSON `error` object and nonzero exit status. `--help` is human-readable. Output files are never replaced without `--overwrite`. Consumed media, edit JSON, transcript, and analysis inputs cannot be used as output destinations, including with `--overwrite`. Paths in an edit are relative to the edit JSON file; exported FCPXML uses absolute file URLs.

## First test

```sh
skywrite init
skywrite defaults image
skywrite models list --task image
skywrite skills read technical-explainer
```

Initialization works offline and preserves existing preferences. Model listing is a free catalog request, not generation. Set `OPENROUTER_API_KEY` in the environment inherited by your agent before paid commands. Use `skywrite defaults image set MODEL_ID` to change a preference; the getter also shows environment overrides. See [configuration](docs/configuration.md), [narration and music](docs/audio-generation.md), and [shared outputs](docs/outputs.md).

Record **ALL** frustrations with `skywrite nit 'description'` while testing.

## Understand media

```sh
skywrite inspect camera-a.mp4 camera-b.mp4 recorder.wav
skywrite analyze camera-a.mp4 --local --frames 12 -o camera-a.local.json
```

`inspect` also reports color metadata for video: FFmpeg's decoded `color_transfer`, `color_primaries`, `color_space`, range and pixel format, container tags, and, for MXF, the raw transfer/primaries/coding-equation labels from the header metadata. Private camera labels (for example Canon's) are shown raw and marked as not publicly decodable; Skywrite never guesses an encoding from pixels. When the operator confirms the capture gamma/gamut, declare it as `sources[].color` in the edit with a matching `sources[].lut`. If a camera's labels look wrong or undecodable, record a `skywrite nit` with the raw `inspect` output and the confirmed settings.

Local analysis creates a manifest and persistent JPEG frames under the shared content-addressed `~/.cache/skywrite/assets/` cache (or XDG/app override). An agent with image tools can inspect the listed frame paths directly. Frame times are source playback seconds. Uniform samples are sparse evidence: they do not establish exact shot boundaries or reveal every event in a long recording.

For optional OpenRouter calls, set `OPENROUTER_API_KEY` in the environment. Discover current capabilities with `skywrite models list --task analysis`, then select `--model MODEL_ID`, `SKYWRITE_ANALYSIS_MODEL`, or a saved default (`skywrite models set-default analysis MODEL_ID`). Legacy `OPENROUTER_VISION_MODEL` is supported. Skywrite does not guess an evolving model ID.

```sh
skywrite transcribe recorder.wav --language en -o recorder.transcript.json
skywrite analyze camera-a.mp4 --model YOUR_VISION_MODEL --prompt 'Find useful interview selects and cutaways' -o camera-a.analysis.json
skywrite analyze recorder.wav --model YOUR_AUDIO_CAPABLE_MODEL --transcript recorder.transcript.json -o recorder.analysis.json
```

Transcription uses OpenRouter's `/audio/transcriptions`, requests `verbose_json` with word timestamps, and sends bounded 16-kHz mono WAV chunks (60 seconds by default; adjustable from 10–120). Responses lacking actual word timestamps are rejected instead of estimating word positions. Non-speech events (for example `[laughs]`) are not stored as words. Words whose audio is near silent, or 30 dB below the chunk's typical word level, carry `audio_level_dbfs` and are counted in `usage.low_audio_words`: on ambient audio these are often recognition hallucinations, so listen before using them. Pass `--language` only when all speech is in that language: Whisper models write speech in other languages translated into the hinted one (a tester's Spanish "con la mano" came back as "With my hand" under `--language en`), so omit it for bilingual footage. Chunk timestamps are shifted into source time, preserving any initial audio-stream delay. If a provider returns words out of order, Skywrite sorts them stably by their actual start times, preserving concurrent words and the provider transcript text; stderr and `usage.word_ordering` report any reordering. Generated transcripts include a source-content SHA-256 fingerprint; analysis rejects them if the source content changes. Manually supplied transcripts without fingerprints are accepted and marked unverified in the coverage report. Text can lose context at chunk boundaries; inspect important edits around those boundaries.

`--start SECONDS --duration SECONDS` limits the selected range for transcription and analysis. Transcription word positions remain relative to the full source, including that starting offset. A transcript supplied to `analyze --transcript` must reference the same source file. Audio-only remote analysis without a transcript sends at most the first 60 seconds of the chosen range and labels that coverage. Video analysis sends sampled frames; include a transcript to supply dialogue.

Explicit remote commands may incur OpenRouter charges. Requests have time, size, frame, and output-token limits, with no automatic paid retries. Exact API request content, model, endpoint, and API base are hashed into the shared `~/.cache/skywrite/responses/` response cache; repeated identical requests reuse it. `--cache-dir PATH` relocates it and `--no-cache` disables response cache reads/writes. Derived frame files are cached separately by source content and sampling settings, so copied files share artifacts. Contextual frame-batch descriptions are cached together; descriptions are not independent per-frame entries. The cache contains media-derived text; delete it when no longer needed. Cached requests can be used without a key. The CLI does not enforce an account-wide dollar budget; use a capped API key.

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

`skywrite schema` prints agent-facing JSON Schema guidance and examples. A layered edit with B-roll over continuous dialogue:

```json
{
  "version": 2,
  "name": "First assembly",
  "width": 1280,
  "height": 720,
  "fps": {"num": 30, "den": 1},
  "video": [
    {"name": "A-roll", "clips": [
      {"start": 0, "source": "camera-a.mp4", "source_in": 2, "duration": 8, "audio": {"volume": 0}},
      {"start": 8, "source": "title.png", "duration": 2, "fade_out": 1}
    ]},
    {"name": "B-roll", "clips": [
      {"start": 3, "source": "camera-b.mp4", "source_in": 20, "duration": 3, "audio": {"volume": 0}}
    ]}
  ],
  "audio": [
    {"name": "Recorder", "clips": [{"start": 0, "source": "recorder.wav", "source_in": 12, "duration": 8, "fade_out": 0.25}]},
    {"name": "Music", "clips": [{"start": 0, "source": "bed.mp3", "duration": 10, "volume": 0.3,
      "volume_keyframes": [{"time": 7.5, "volume": 1}, {"time": 8.5, "volume": 2}], "fade_out": 1.5}]}
  ],
  "master": {"limiter_db": -1}
}
```

Every clip has an explicit timeline `start`. Video track 0 is the bottom layer; upper tracks composite above it with transparent letterboxing, optional `opacity` and picture fades. Empty timeline is black and silent. A video clip's own sound is controlled by its `audio` object; sound-only media go on `audio` tracks. `source_in` and `duration` are seconds; **duration is output duration**, and a clip consumes `duration * speed` source seconds. Volumes are linear gains (0 mutes) with optional keyframes and fades. `sources[]` can declare an operator-confirmed color encoding and a `.cube` LUT for the render. Version 1 edits are rejected; convert them with `skywrite edit migrate OLD.json --output NEW.json`. See [edit format](docs/edit-format.md).

Validation checks real source files, source ranges, overlaps within a track, finite positive numbers, supported rates, stream types, LUT files and even dimensions. Starts and durations are rounded to the rational frame grid, with warnings in the validation result. Use `fps={"num":30000,"den":1001}` for NTSC timing. Sources use their presentation timeline; changing a sample-rate label or frame-rate declaration does not itself prove clock drift.

An external agent can write the JSON itself, or request a model draft from saved analyses:

```sh
skywrite edit suggest --analysis camera-a.analysis.json recorder.analysis.json --brief 'Make a concise opening with complete thoughts' --model YOUR_TEXT_MODEL -o edit.json
skywrite edit validate edit.json
```

Suggestions use only analyzed sources and are validated before being written. Models still need editorial review; sparse observations cannot guarantee a good cut. The deterministic renderer never executes generated Python/JavaScript or asks a model to hand-author FCPXML.

## Describe frames and video

```sh
skywrite describe frames camera-a.mp4 --every 30 -o survey.json
skywrite describe frames camera-a.mp4 --around 125.4 --window 1 --cast cast.json -o cut.json
skywrite describe video camera-a.mp4 --start 56 --duration 8 -o moment.json
```

Frame descriptions are cached per exact frame, model, prompt and cast, and selected on the source's absolute frame grid, so a dense pass around a candidate cut reuses everything a coarse survey already described. A cast file of text appearance descriptions adds advisory "who is visible" hints (no face recognition). Video descriptions report timed gestures, expressions and shot changes from a short re-encoded range. Defaults (GPT-6 Luna for frames, Gemini 3.1 Flash Lite for video, Scribe v2 for verbatim transcription) come from a measured [model evaluation](docs/model-evaluation.md). Understanding requests use zero-retention providers by default. See [descriptions](docs/describe.md).

## Review an edit

```sh
skywrite review sheet camera.mp4 --start 120 --duration 1 --every 2 -o around-cut.jpg
skywrite review cuts edit.json --handles 1.5 --output-dir cut-review
skywrite review lint edit.json --transcript presenter.transcript.json
```

Contact sheets tile consecutive exact frames with source times, so expressions and motion around a candidate cut are visible rather than guessed from sparse samples. Cut auditions are short clips of the rendered edit around every cut for watching and listening. Lint reports objective problems: a visible speaker with no audible dialogue, repeated words at or inside a cut, words clipped by a cut, filler words, sound cut off mid-flow (applause, music tails), and beds close to dialogue level. None of these judge taste. See [reviewing an edit](docs/review.md).

## Export and render

```sh
skywrite edit export edit.json --to premiere -o edit.xml
skywrite edit export edit.json --to fcpxml -o edit.fcpxml
skywrite edit render edit.json -o preview.mp4
skywrite edit export edit.json --to ffmpeg -o render.py
python3 render.py --output preview.mp4
skywrite edit export edit.json --to hyperframes -o hyperframes-project
skywrite edit export edit.json --to remotion -o remotion-project
```

- **Import:** `skywrite edit import premiere.xml -o edit.json` brings an editor's Premiere/FCP 7 XML timeline back as a layered edit (layers, linked sound, speed, opacity, levels, fades, markers); `.prproj` is read experimentally.
- **Premiere XML:** writes Final Cut Pro 7 XML (xmeml v5), which Premiere Pro imports. Video tracks, linked sound, audio tracks, speed (Time Remap), opacity/fades and gain keyframes (Audio Levels) are carried; whole-frame timing rounds subframe in points, with a warning. Structure and timing are checked with OpenTimelineIO's independent reader; import into Premiere itself is not yet verified, so report results with `skywrite nit`.
- **FCPXML:** generates an FCPXML 1.10 timeline with rational timing, original-media references, a connected storyline per track, constant retiming, blend and volume keyframes. Actual Final Cut/Resolve import must be checked in those applications; generating XML on Linux cannot certify application import fidelity.
- **Native render:** calls installed FFmpeg, composites layers, applies LUTs, opacity, fades, gain keyframes and the optional master limiter, conforms constant speed, mixes audio, and creates an MP4. It preserves the originals. The generated Python renderer uses only the Python standard library and FFmpeg, with the same compiled edit operations. NLE exports warn about LUTs and the limiter, which must be applied in the NLE to match.
- **Hyperframes:** exports a new HTML composition project and copied assets for the installed CLI. Follow the generated README for lint/preview/render. Existing project directories are never overwritten.
- **Remotion:** exports a new React composition project and copied assets. Install the generated npm dependencies inside that project, then preview/render using its scripts. This optional backend currently requires integer fps; use FCPXML/FFmpeg for rational fps. Remotion's own runtime and licensing apply.

Unsupported target behavior is reported rather than silently discarded. Exported projects are starting points an agent can extend with graphics and effects; the shared assembly format intentionally stays small.

## Native still graphics with upstream Takumi

```sh
skywrite image render --file title.html -o title.png --width 1280 --height 720
skywrite image render --file card.json -o card.webp --quality 90 --font ./Brand.ttf
```

The renderer embeds original upstream `takumi = 2.19.0` with its `from-html` feature, not a CLI fork. No external Takumi executable, browser, FFmpeg, or system fonts are needed for this command. Rust 1.91 or newer is required to build Skywrite. An unmodified Geist variable font is embedded under its [SIL Open Font License](assets/fonts/OFL.txt); use repeated `--font PATH` to register extra local font files. Select their family in your template. Glyph coverage follows the registered fonts.

Templates are `.html`/`.htm` fragments or upstream Takumi JSON nodes, for example `{"type":"text","text":"Hello","style":{"fontSize":48,"color":"white"}}`. HTML supports upstream inline `style` and `tw` attributes. The stable upstream HTML entry point discards stylesheet elements, so Skywrite rejects `<style>`/`<link>` inputs with guidance to use inline styles. HTML error recovery follows upstream; this is a layout renderer with no JavaScript execution. PNG preserves alpha; WebP/JPEG accept `--quality 0..100` (default 75); PNG rejects quality because it is lossless. The format is inferred from the output extension unless supplied explicitly.

Image-node `src` paths resolve relative to the template. Only local raster images are supported; remote URLs, data URIs, inline image buffers and SVG sources are unsupported. CSS background image resources are outside this interface. Rendering makes no network requests. Templates are limited to one MiB, 4096 rendered nodes and depth 64, with at most 16 megapixels on a canvas whose sides are 1..8192. Local images have the same per-image pixel budget and at most 32 megapixels combined; each font/image is at most 16 MiB, with 32 MiB combined resource bytes, at most 64 image resources and sixteen extra fonts. These are input bounds, not a hard operating-system memory or CPU quota.

Native outputs are published atomically, existing files require `--overwrite`, and the template, loaded fonts and loaded images cannot be replaced by the output. Include resulting stills in the edit's primary clips.

## Validation

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

Integration tests require installed FFmpeg/ffprobe and exercise actual decoding, rendering, and known timing transformations. Paid network tests are explicit/manual and use a capped OpenRouter key. Dated live checks cover transcription/understanding, image generation, H3/H3 Max video, video upscaling, narration, and Lyria Clip music; R2 checks use an explicitly configured test namespace. Do not confuse local/cached protocol tests with a successful provider call.

The public AMI corpus (CC BY 4.0) is suitable for multi-microphone synchronization tests; controlled copies with known trims/rates provide ground truth. `https://groups.inf.ed.ac.uk/ami/download/` lists downloads and attribution. Synthetic fixtures are generated locally for deterministic regression checks. See [measured validation and limits](docs/validation.md). Final independent review is still in progress; these checks are not production or all-platform certification.

## Agent workspace and reusable workflows

Record **ALL** agent frustrations, bugs and missing features with `skywrite nit 'description'`. This temporary feedback command saves local JSON files in `~/.skywrite/nits/`; nothing is uploaded. Your editor can share that folder. Save creative prompts with `skywrite styles create NAME --content 'prompt'` and read/update them later. `skywrite skills list` discovers built-in Hyperframes, Remotion, FCPXML, FFmpeg, Takumi, sync and workflow instructions; personal Markdown skills can shadow them explicitly. `SKYWRITE_HOME` relocates personal files. See [workspace details](docs/workspace.md).

Tools can be composed: render a native HTML graphic, add its PNG to a Resolve/FCPXML edit, and use a Remotion/Hyperframes project for animation. Generation workflows can build a character image, derive reference frames and supply them to a compatible video model; always check actual reference capabilities before requesting a paid job.

Google Fonts are explicit: `skywrite image render --file title.html -o title.png --google-font 'Inter:wght@400;700'`. Downloads are cached and `--offline` uses cached variants. Takumi supports upstream `tw` utilities and inline styles, not a full browser or arbitrary Tailwind stylesheet compiler. See [fonts](docs/fonts.md), and [shared cache, model discovery and task defaults](docs/cache-models.md).

Generation and upscaling are available through `skywrite generate image|video`, `skywrite upscale image|video`, and `skywrite jobs list|status|resume`. Reuse character references, animate a keyframe, then include generated media in the validated edit format. Durable jobs retain provenance/cost and prevent automatic duplicate paid submissions. Choose a task model explicitly or save its default; preview with `--dry-run`.

AI video upscaling needs an accessible HTTPS source: use `--source-job` for generated video, `--source-url` for a hosted source, or `--upload` with your configured public S3/R2 bucket. OpenRouter currently rejects inline local video data URLs for this operation. `upscale image` is explicitly non-AI FFmpeg Lanczos resizing. See [generation and jobs](docs/generation.md) for commands and recovery limits.

Public S3/R2 sources for local video upscaling are implemented behind explicit `--upload`, with hash reuse, optional `--delete-upload-after`, and scoped `skywrite storage purge`. A live Cloudflare R2 pass on 2026-10-30 verified upload reuse, R2-backed upscaling, cleanup recovery, scoped purge, and cross-machine output transfer. Amazon S3 itself and other compatible providers were not tested. See [storage setup and cleanup](docs/storage.md).

## Setup and generated narration/music

`skywrite init` creates offline starter preferences without replacing existing choices. `skywrite defaults image` shows saved/effective choices and their origin; `skywrite defaults image set MODEL` changes the saved preference. Environment overrides still apply. See [configuration](docs/configuration.md).

Discover voices with `skywrite models list --task speech-generation` and music with `skywrite models list --task music-generation`. Generate narration with `skywrite generate speech --file narration.txt --model hexgrad/kokoro-82m --voice af_heart --output narration.mp3`; create a bed with `skywrite generate music --model google/lyria-3-clip-preview --prompt 'Original gentle electronic music' --instrumental --output bed.mp3`. Audio uses the same durable no-duplicate job system and is fully decoded before completion. Music token prices of zero are not a free-song offer.

Generate a whole asset graph with `skywrite project init story.json`, then `project validate`, `project run`, `project status`, and `project resume`. [Generation projects](docs/projects.md) freeze models, bound concurrency, preserve inputs and outputs, and recover existing job receipts without resubmitting uncertainty. `project schema` emits the complete JSON contract; [the example](examples/generation-project.json) connects a character image to its video first frame.

Read [audio generation and recovery](docs/audio-generation.md) and `skywrite skills read technical-explainer` for narration, real word timing, deterministic diagrams, animation, and mixing music into an edit. MIDI/text-score rendering is deferred. Transfer finished assets between machines with [shared outputs](docs/outputs.md).
