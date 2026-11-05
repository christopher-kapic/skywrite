# Narration and music

`skywrite generate speech` and `skywrite generate music` use the same persistent job records, request identities, explicit model selection, `--take`, no-clobber outputs, and `jobs resume` behavior as image/video generation. No MIDI, XML-to-MIDI, soundfont, or local score renderer is required or included.

## Models and voices

```sh
skywrite models list --task speech-generation
skywrite models list --task music-generation
skywrite defaults speech-generation set hexgrad/kokoro-82m
skywrite defaults music-generation set google/lyria-3-clip-preview
```

Selection is explicit `--model` before task environment and saved defaults. The task environments are `SKYWRITE_SPEECH_GENERATION_MODEL` and `SKYWRITE_MUSIC_GENERATION_MODEL`. Speech catalog rows contain `supported_voices` when the provider supplies a list. Voice IDs belong to a model; an ID from another model may not work.

The first narration adapter requests MP3 from `/audio/speech`. It supports MP3-capable speech providers, including the tested Kokoro model. Google Gemini TTS and HeyGen currently document PCM-only output and are rejected before a paid request by this surface. WAV publication is supported by converting a valid MP3 response; raw PCM is never mislabeled as MP3/WAV. Voice cloning and multi-speaker turns are not part of this first narration command. Seed Audio treats its input as a generative prompt, so select a conventional TTS model when you need verbatim narration. Delivery instructions may be ignored by a provider. Non-default speed is accepted only for adapters with documented compatible speed control.

Current music discovery selects Google's Lyria models from the audio-output catalog, rather than treating conversational GPT Audio as a music composer. `google/lyria-3-clip-preview` advertises roughly 30-second clips; `google/lyria-3-pro-preview` advertises full songs. Their documented per-clip/song prices are separate from generic token prices. **Zero prompt/completion token prices do not mean free music.** Inspect actual returned usage and the model page; absent billing data is unknown, never zero. Model availability and prices can change.

## Generate assets

```sh
skywrite generate speech --text 'Welcome to this explanation.' \
  --model hexgrad/kokoro-82m --voice af_heart --output narration.mp3

skywrite generate speech --file narration.txt \
  --model hexgrad/kokoro-82m --voice af_heart --output narration.wav

skywrite generate music --model google/lyria-3-clip-preview \
  --prompt 'Original gentle electronic bed, warm chords, light marimba pulse, 96 BPM, clean ending' \
  --instrumental --output bed.mp3
```

A music reference image can be supplied with `--reference image.png` when the selected model advertises image input. Narration scripts may be up to 64 KiB; music prompts up to 32768 bytes; delivery instructions up to 4096 bytes. Each audio response is limited to 32 MiB and 600 seconds.

## Long narration

A script too long for one request is generated in sections and joined into the single `--output` file; no extra files are written to your folders. Splits fall on paragraph breaks (blank lines), packing whole paragraphs together while they fit; a paragraph that is too long on its own falls back to sentence ends, then words. `--paragraph-pause SECONDS` (default 0.3, range 0–5) is the silence added at paragraph seams; sentence seams inside one paragraph get none. The pause only applies to split scripts, because a single request keeps the model's own pacing.

The per-request budget comes from the most specific source available, reported as `split.budget_source`: a limit Skywrite knows (Seed Audio: 3000 characters), else the catalog's `context_length` (about two characters per token), else a conservative 2000 characters. Every request also stays within 8192 bytes. Speech is usually billed by input length, so more sections cost about the same as one long request; `--dry-run` lists the sections and their job IDs without paying. If a provider clearly rejects a section as too long (HTTP 400/413/422 naming a length limit) before generating, that section is split in half at sentence ends and resubmitted as new, smaller requests. Uncertain outcomes are never retried.

Each section is an ordinary durable job keyed by its own text, voice, model, instructions and `--take`. If a section fails, rerun the same command: finished sections replay from the job store and only the missing ones are requested. Sections are decoded to 48-kHz mono, joined sample-exactly, and encoded once. The result lists `sections` with `start_seconds`, `end_seconds`, `break_before`, `job_id` and the first words of each, so visuals can follow narration sections without separate files. `usage.cost` is the sum over sections only when every section's billing is known. The joined result is limited to one hour. Speech nodes in `skywrite project` manifests record one job each, so they must fit one request; split long project narration into several speech nodes.

Music is received from streaming `/chat/completions`; each `delta.audio.data` base64 chunk is decoded separately, then joined in order. A complete stream terminator and nonempty audio are required. The provider may return MP3 even when a different format was requested, so Skywrite detects and fully decodes the actual container before declaring completion. Original MP3/WAV artifacts remain in the job folder. `.mp3`/`.wav` publication converts as necessary; it does not rename raw bytes. WAV publication uses 48-kHz stereo PCM16. Existing output replacement always requires `--overwrite`; input scripts and references remain protected.

Use [`skywrite audio`](audio-finishing.md) for local two-pass loudness or peak normalization, pitch-preserving tempo, music loops with crossfades, narration-driven ducking, fades, trimming and exact silence padding. These steps reuse FFmpeg and make no paid requests. Reports include actual decoded final samples; generated MP3 metadata alone is not a reliable editing endpoint.

Generated music is not loudness-managed and can clip: a Lyria 3 Clip bed measured a +0.81 dBFS decoded peak. Check `skywrite inspect --exact-audio` and normalize with `skywrite audio` before mixing it under narration.

`--instrumental`, tempo, key, structure and duration are creative requests, not verified promises. Listen to music for unwanted vocals, unsuitable dynamics, and the ending. The first tested clip measured about 27.3 seconds despite its nominal 30-second tier. These outputs are waveform files; editable MIDI-score tooling is deferred.

## Recovery and billing

The job intent is fsynced before a paid POST. Speech response headers, including a generation ID when supplied, are saved before reading the binary body; a partial response leaves an uncertain outcome, not an automatic retry. Complete responses are saved before decode/completion, allowing local recovery if publication or validation fails. Invalid audio stays in the received state with its response and billing metadata intact; it is never published as a completed result.

```sh
skywrite jobs list
skywrite jobs status JOB_ID
skywrite jobs resume JOB_ID --output narration.mp3
skywrite jobs status JOB_ID --refresh
```

A completed same-format replay needs neither an API key nor network/FFmpeg. Conversion needs FFmpeg. `--refresh` may query OpenRouter; speech billing can settle after the audio response, so an early job can have `usage: null`. Refresh can retrieve the later reported cost from generation metadata. Check activity for an uncertain submission before intentionally using a new `--take`; doing so is a new paid variation, not an automatic retry. Narration and music return their audio in the response itself, so there is no provider job ID to adopt later: an uncertain audio job cannot be recovered by `jobs resume`, which exits with an error (writing nothing) for uncertain, rejected and failed jobs. A completed job whose `usage` is still null carries a `usage_note`; its cost is unknown, not zero.

## Measured live checks

On 2026-10-30, Kokoro narration and Lyria 3 Clip music produced decoded audio through the CLI, including MP3-to-WAV publication and completed-job replay without keys or network. Lyria Pro was discovered in the catalog but was not live-generated in this pass. See [validation](validation.md) for measured formats, costs and the separate H3 video check. These results do not guarantee other models’ availability or artistic output.

## Technical explainers

Read `skywrite skills read technical-explainer`. Check a script's facts first; generate narration, then use `transcribe` for actual word timings. Render precise labels/diagrams with Takumi and animate them with Hyperframes or Remotion. Place narration and a quieter music bed in the edit's audio tracks and export/render normally. Generated footage can supplement deterministic technical graphics. See [edit format](edit-format.md), [configuration](configuration.md), and [shared outputs](outputs.md).

Protocol references: [OpenRouter speech](https://openrouter.ai/docs/guides/overview/multimodal/tts), [streaming audio output](https://openrouter.ai/docs/guides/overview/multimodal/audio), and [music models](https://openrouter.ai/collections/audio-models).
