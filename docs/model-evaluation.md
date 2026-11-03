# Model evaluation, 2026-11-02

Skywrite's starter defaults for frame description, video description and
transcription come from a measured comparison of cheap OpenRouter models on the
AMI Meeting Corpus (ES2002a close-ups, corner camera and headset mix; CC BY 4.0,
consenting participants, with manual annotations) and a Big Buck Bunny trailer
(CC BY 3.0) with real cuts. All requests used `provider.data_collection: deny`.
Total spend was $0.48. Small samples: treat the numbers as directional.

## Frame description (22 frames, one frame per request)

| Model | Invented details | People count accuracy | Mid-speech (right/wrong/abstain of 11) | $ per 1,000 frames |
|---|---|---|---|---|
| **openai/gpt-6-luna** (default; reasoning off) | **0** | 0.84 | 8/1/2 | 0.22 |
| **google/gemini-3.1-flash-lite** (runner-up) | 2 | 0.95 | 11/0/0 | 0.80 |
| z-ai/glm-5.3-flash | 4 | 0.95 | 10/1/0 | 0.12 |
| qwen/qwen3.7-flash (no zero-retention provider) | 5 | 0.95 | 11/0/0 | 0.07 |
| google/gemini-3.5-flash-lite (analysis default) | 1 | 0.84 | 10/1/0 | 1.14 |
| amazon/nova-2-lite-v1 | 2 | 0.90 | 7/3/1 | 0.76 |

GPT-6 Luna was the only model with no invented details, and on a near-black frame it
answered "details unclear" instead of inventing figures. It miscounts people in
fisheye wide shots. A one-frame-per-second survey of an hour at 25 fps costs about
$0.66 with Luna.

## Cast matching from text descriptions (38 frames, two conditions)

With distinct descriptions, every model reached precision 1.00 on close-ups, never
claimed a person on an empty seat, and never swapped two participants. With
look-alike decoys (each differing in one attribute), several models confidently
picked the decoy when the deciding feature was not visible, for example labelling a
man seen from behind as the "no glasses" decoy, or inventing "clean-shaven" for a
bearded man. Wrong matches came at 0.8–0.9 confidence. GLM-5.3-flash and GPT-6 Luna
made zero decoy confusions (at the cost of low recall in wide shots). Skywrite
therefore presents cast matching as advisory, asks for the deciding feature, and
documents the failure modes.

## Video description (inline base64 MP4)

Inline local video worked on chat completions for all eight video-capable models up
to 60 s. A 30 MB request failed on Google AI Studio (20,000,000-byte limit), Bedrock
and Seed. **google/gemini-3.1-flash-lite** (default) found every shot change within
0.2 s and timed gestures correctly, with zero-retention routing, at about $0.0017
per minute of 480p video. GLM-5.3-flash gave finer event timing on short clips but
samples a fixed number of frames regardless of length.

## Verbatim transcription (three dense 60-second meeting chunks)

| Model | WER | Filler recall | Repetition recall | Median start error | $ per audio minute |
|---|---|---|---|---|---|
| **elevenlabs/scribe-v2** (default) | **0.158** | **0.67** | **0.90** | 0.05 s | 0.0037 |
| nvidia/parakeet-tdt-0.6b-v3 | 0.201 | 0.63 | 0.70 | 0.09 s | 0.0015 |
| openai/whisper-large-v3 (previous default) | 0.277 | 0.26 | 0.20 | 0.115 s | 0.00045 |

The well-known Whisper `prompt` trick for keeping disfluencies had no effect through
OpenRouter (byte-identical output), and routing varied between providers, one of
which drops all fillers. Scribe v2 keeps "um", restarts and repeated words (the
"Evanston Evanston" case) and returns tight word timestamps. It also returns
non-speech events such as `[laughs]`, which Skywrite excludes from words.

## Data policy findings

Every candidate had a provider honouring `data_collection: deny`, but several such
routes are not zero-retention (for example qwen3.7-flash and gemini-2.5-flash-lite).
Skywrite therefore also requests zero data retention by default; GPT-6 Luna then
routes via Azure and the Gemini models via Vertex.
