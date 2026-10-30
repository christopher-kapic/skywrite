# Model preferences and first setup

Agents can inspect configuration without calling a model:

```sh
skywrite defaults
skywrite defaults image
skywrite models list --task image
skywrite defaults image set black-forest-labs/flux-3-image-klein
skywrite defaults image unset
```

`default` is an alias for `defaults`. The short task names `image`, `video`,
`speech`, and `music` refer to their corresponding `*-generation` tasks. Full
task names work throughout model discovery and default inspection. Existing
`models defaults`, `models set-default TASK MODEL`, and `models unset-default`
commands remain available with full task names.

The JSON response identifies the effective model, its source, the saved
preference, and the environment variable that can override it. For example,
setting an image preference does not override an existing
`SKYWRITE_IMAGE_GENERATION_MODEL`. A setter stores a model ID; it does not pay
for inference or validate the ID against a remote catalog. Use `models list`
to discover currently advertised models and capabilities.
An invalid environment override is reported as `invalid_override` with a null
effective model and a diagnostic. Configuration inspection and saving still
work, so an agent can help correct it; actual media commands reject that invalid
override before sending a request.

Selection precedence is:

1. An explicit `--model` on the media command.
2. `SKYWRITE_<TASK>_MODEL`, with dashes in the task changed to underscores.
3. `OPENROUTER_VISION_MODEL` for analysis and editing, for compatibility.
4. The saved task preference.
5. The existing `openai/whisper-large-v3` transcription fallback.

An unset task is reported with a null model; it does not silently select a paid
model. Image upscaling currently uses local FFmpeg Lanczos resizing. Its query
reports that local mode, and any saved image-upscaling preference is reserved
for a future model-backed implementation rather than used by the current tool.

## Offline starter configuration

```sh
skywrite init --dry-run
skywrite init
skywrite defaults video
skywrite defaults video set minimax/hailuo-3
```

`init` adds missing model preferences from a dated starter set. It preserves
existing preferences and unrelated config fields, and a second run is
idempotent. Concurrent init and setter processes use the same stable lock and
atomic config publication. A dry run does not create a config or lock file.
Initialization works offline, stores no API key, does not access cloud storage,
and makes no paid request. Malformed or oversized existing config is rejected
without replacement.

The starter choices are intentionally stable preferences rather than an
automatically changing ranking: Gemini Flash Lite for analysis/editing,
Whisper for word transcription, FLUX 3 Image Klein for images, H3 Max for video,
FLUX Video Upscale, Kokoro for speech, and Lyria 3 Clip for music. The command
prints the exact IDs, curation date, and reasons. Model availability and prices
can change; `init` does not guarantee current provider availability. Use
`models list --refresh --task TASK` before a new paid workflow. Speech also needs
a voice supported by the selected model. Music's zero generic token prices do
not mean free clips or songs.

Preferences live in `~/.skywrite/config.json`; `SKYWRITE_HOME` changes the
personal directory. For paid requests, set `OPENROUTER_API_KEY` in the
environment inherited by the agent process. An export in a different terminal
does not update an already running agent. Storage settings and credentials are
separate; `init` does not create buckets or enable public access.
