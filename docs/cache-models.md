# Cache and model selection

`skywrite analyze` sends a small uniformly sampled set of timestamped JPEGs to an explicitly selected model. A transcript supplies dialogue. Audio-only analysis without a transcript sends a bounded audio sample. This is sparse evidence: descriptions do not establish exact shot boundaries or unseen actions.

Regeneratable data lives under `$XDG_CACHE_HOME/skywrite` (normally `~/.cache/skywrite`). A relative `XDG_CACHE_HOME` is ignored. Override with `SKYWRITE_CACHE_DIR` or `--cache-dir` (highest priority). Personal styles, skills, nits, defaults and generation jobs belong in `~/.skywrite`, separate from this cache. `SKYWRITE_HOME` relocates personal data.

- `assets/<file-sha256>/frames/<sampling-key>/`: JPEGs and an integrity manifest. Keys include range, count, sizing/quality and frame format version. File content is hashed, not just its name/mtime. Copies at different paths share frames. Extraction is serialized per source hash and complete directories are published atomically. Hashes are verified on reuse; damaged entries report an error and can be deleted explicitly. `--frames-dir` overrides the asset root.
- `responses/<request-sha256>.json`: exact OpenRouter request/response cache. Model, endpoint, API base, prompt, media bytes and semantic source metadata determine identity. Analysis prompts exclude machine-specific paths and include source SHA-256, so identical copies reuse contextual descriptions. Current report paths refer to the current source. A batch is described in context; descriptions are not independently cached per frame.
- `models/`: free catalog responses with a 24-hour TTL. `--refresh` fetches current catalogs, `--offline` requires a cached catalog and reports staleness. Discovery uses no paid inference and attaches no API key to GETs.
- Google Fonts are cached under `fonts/`; see [fonts.md](fonts.md).

`--no-cache` on understanding/transcription/edit commands disables **response** cache reads/writes. It does not remove frame artifacts. Response cache reuse needs no API key. Skywrite never automatically retries a paid request. `OPENROUTER_API_KEY` is read from the process environment, never written to config; `.env` files are not automatically loaded. The cache contains derived private media/text; delete it when appropriate. There is no automatic eviction yet.

An analysis transcript must reference the same file, or carry a matching `usage.source_sha256` proving content identity with the selected copy. A supplied hash that differs is always rejected. Legacy transcripts without hashes still require a matching canonical source path.

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

Tasks: `analysis`, `transcription`, `editing`, `image-generation`, `video-generation`, `image-upscaling`, `video-upscaling`. Selection order: explicit `--model`, task environment variable (`SKYWRITE_ANALYSIS_MODEL`, `SKYWRITE_IMAGE_GENERATION_MODEL`, etc.), saved default, then the transcription-only fallback `openai/whisper-large-v3`. Legacy `OPENROUTER_VISION_MODEL` remains a fallback environment variable for analysis/editing, preceding saved defaults. Skywrite never silently chooses a supposedly best image/video model. Setting a default stores an ID and incurs no charge.

Dedicated image/video catalogs expose reference-frame support, resolutions, durations and provider parameter limits. Generic metadata supplies modalities and prices when available. Video prices must be read from `pricing_skus` with their units: zero generic prompt/completion prices do **not** mean free video. Unknown pricing is not zero. Capabilities are provider claims, not compatibility tests. Transcription modality alone does not guarantee word timestamps. `image-upscaling` discovery includes explicitly identifiable upscale models; an empty result explains that high-resolution generative editing is not faithful upscaling.

Official references: [XDG directories](https://specifications.freedesktop.org/basedir/latest/), [OpenRouter models](https://openrouter.ai/docs/api/api-reference/models/list-all-models-and-their-properties), [video model discovery](https://openrouter.ai/docs/guides/overview/multimodal/video-generation).
