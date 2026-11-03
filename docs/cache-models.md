# Cache and model selection

`skywrite analyze` sends a small uniformly sampled set of timestamped JPEGs to an explicitly selected model. A transcript supplies dialogue. Audio-only analysis without a transcript sends a bounded audio sample. This is sparse evidence: descriptions do not establish exact shot boundaries or unseen actions.

Regeneratable data lives under `$XDG_CACHE_HOME/skywrite` (normally `~/.cache/skywrite`). A relative `XDG_CACHE_HOME` is ignored. Override with `SKYWRITE_CACHE_DIR` or `--cache-dir` (highest priority). Personal styles, skills, nits, defaults and generation jobs belong in `~/.skywrite`, separate from this cache. `SKYWRITE_HOME` relocates personal data.

- `assets/<file-sha256>/frames/<sampling-key>/`: JPEGs and an integrity manifest. Keys include range, count, sizing/quality and frame format version. File content is hashed, not just its name/mtime. Copies at different paths share frames. Extraction is serialized per source hash and complete directories are published atomically. Hashes are verified on reuse; damaged entries report an error and can be deleted explicitly. `--frames-dir` overrides the asset root.
- `responses/<request-sha256>.json`: exact OpenRouter request/response cache. Model, endpoint, API base, prompt, media bytes and semantic source metadata determine identity. Analysis prompts exclude machine-specific paths and include source SHA-256, so identical copies reuse contextual descriptions. Current report paths refer to the current source. A batch is described in context; descriptions are not independently cached per frame.
- `models/`: free catalog responses with a 24-hour TTL. `--refresh` fetches current catalogs, `--offline` requires a cached catalog and reports staleness. Discovery uses no paid inference and attaches no API key to GETs.
- Google Fonts are cached under `fonts/`; see [fonts.md](fonts.md).

`--no-cache` on understanding/transcription/edit commands disables **response** cache reads/writes. It does not remove frame artifacts. Response cache reuse needs no API key. Skywrite never automatically retries a paid request. `OPENROUTER_API_KEY` is read from the process environment, never written to config; `.env` files are not automatically loaded. The cache contains derived private media/text; delete it when appropriate. There is no automatic eviction yet.

An analysis transcript must reference the same file, or carry a matching `usage.source_sha256` proving content identity with the selected copy. A supplied hash that differs is always rejected. Legacy transcripts without hashes still require a matching canonical source path. Saved analyses and embedded transcripts are checked again against current media before `edit suggest` makes a model request. `edit suggest --transcript words.json` can attach existing source-matched words to older analyses without reanalysis or ASR; repeat the flag for multiple sources. Conflicting embedded or explicit word alignments are rejected; identical evidence is idempotent. Legacy path-only transcripts are labeled as lacking content identity verification. The suggestion context includes original words overlapping each analyzed range, including whole words that cross its boundaries, rather than only a dialogue summary. Word timestamps are source playback seconds and retain their original precision. Per-range limits are 8,192 words and 512 KiB of word JSON; combined suggestion context is limited to eight MiB. Excess context fails with guidance to narrow the analysis range rather than silently dropping words. Sparse visual sample times do not establish dialogue boundaries. Legacy analyses without a fingerprint cannot establish historical content identity from a path alone.

```sh
skywrite models list --task analysis --input image
skywrite models list --task image-generation --refresh
skywrite models list --task video-generation --search hailuo
skywrite models list --task video-upscaling
skywrite models list --task transcription --offline
skywrite models set-default analysis vendor/model
skywrite models defaults
skywrite models unset-default analysis
```

Tasks: `analysis`, `frame-description`, `video-description`, `transcription`, `editing`, `image-generation`, `video-generation`, `image-upscaling`, `video-upscaling`, `speech-generation`, `music-generation`. Short aliases `image`, `video`, `speech`, and `music` are accepted. Selection order: explicit `--model`, task environment variable (`SKYWRITE_ANALYSIS_MODEL`, `SKYWRITE_IMAGE_GENERATION_MODEL`, etc.), saved default, then the transcription-only fallback `elevenlabs/scribe-v2`. Legacy `OPENROUTER_VISION_MODEL` remains a fallback environment variable for analysis/editing, preceding saved defaults. Skywrite never silently chooses a supposedly best image/video model. Setting a default stores an ID and incurs no charge. `skywrite defaults image` explains its effective source, `skywrite defaults image set MODEL` saves a preference, and `skywrite init` adds missing offline starter choices without replacing existing preferences. See [configuration setup](configuration.md).

Dedicated image/video catalogs expose reference-frame support, resolutions, durations and provider parameter limits. Generic metadata supplies modalities and prices when available. Video prices must be read from `pricing_skus` with their units: zero generic prompt/completion prices do **not** mean free video. Unknown pricing is not zero. Speech catalogs include supported voice IDs; music uses song/clip pricing, so zero generic audio token rates are not free music. Capabilities are provider claims, not compatibility tests. Transcription modality alone does not guarantee word timestamps. `image-upscaling` discovery includes explicitly identifiable upscale models; an empty result explains that high-resolution generative editing is not faithful upscaling.

Official references: [XDG directories](https://specifications.freedesktop.org/basedir/latest/), [OpenRouter models](https://openrouter.ai/docs/api/api-reference/models/list-all-models-and-their-properties), [video model discovery](https://openrouter.ai/docs/guides/overview/multimodal/video-generation).

Edit suggestions report advisory dialogue cut warnings on stderr when audible source boundaries fall inside supplied words. Muted clips and contiguous matching source audio are exempt. Warnings do not change or reject intentional edits, and missing warnings do not establish perfect provider alignment.
