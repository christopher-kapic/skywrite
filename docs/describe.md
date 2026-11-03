# Frame and video descriptions

`skywrite describe` asks a vision model about exact frames or a short video range.
Requests carry your media, so they follow the [data policy](configuration.md#data-policy-for-media-understanding)
(zero retention by default). Discover models with `skywrite models list --task frame-description`
or `--task video-description`; defaults come from `skywrite init` and the
[model evaluation](model-evaluation.md).

## Frames: survey, then focus in

```sh
# Coarse survey: one frame per second (every 30th frame at 30 fps)
skywrite describe frames interview.mp4 --every 30 -o survey.json
# Dense pass around a candidate cut: every 2nd frame for one second
skywrite describe frames interview.mp4 --around 125.4 --window 1 -o cut-125.json
# Check counts and cache hits first; makes no requests
skywrite describe frames interview.mp4 --interval 2 --dry-run
```

Each frame is described independently from one still (fit to 768 px). Frames are
chosen on the source's absolute frame grid (`--every N` keeps frames whose number is
a multiple of N; `--interval S` keeps the first frame of each S-second bucket), so a
dense pass reuses every frame an earlier survey already described. Results are
cached per source content hash, exact presentation time, model, prompt version and
cast. Re-running, widening or overlapping a pass never pays twice, and a run with
only cached frames needs no API key. Failed frames are not cached; rerun the same
command to retry them alone.

Each result lists the source time, estimated frame number, the persistent JPEG that
was described (under the cache's `assets/<sha>/exact-frames/`), and a JSON
description: shot size, camera angle, people (position, appearance, gaze,
expression, mouth, apparent mid-speech, gesture), setting, objects, legible text,
image issues and a one-sentence summary. `--transcript words.json` attaches the
words spoken around each frame without sending them to the model.

`--max-frames` (default 600) refuses runs that would describe more uncached frames;
raise it deliberately for long sources. `--concurrency` (default 4, up to 16) sets
parallel requests. The report includes the billed cost of new descriptions.

A useful cut workflow:

1. Transcribe for word timestamps.
2. Survey with `--every 30` (or `--interval`).
3. Where a cut might go, run `--around TIME --window 1`, and view the frames or a
   `skywrite review sheet` of the same range.
4. Choose the cut from the words and the frames, then audition it with
   `skywrite review cuts`.

## Who is visible: cast files

```json
{"version": 1, "people": [
  {"name": "Ralph", "description": "white hair, grey suit, red tie; usually at the podium"},
  {"name": "Host", "description": "long dark hair, green dress"}
]}
```

`--cast cast.json` adds advisory matching to each description: `cast_matches` (with
the deciding feature the model saw and a runner-up) and `cast_present`. Matching
uses only your text descriptions: no reference photos, no face recognition, and no
biometric data. Treat results as hints, not identification. In evaluation it was
reliable with distinct descriptions in close-ups, but it can pick a look-alike when
the deciding feature isn't visible (glasses from behind, colours in a tiny wide
shot), and the model's confidence does not reflect that. Describe stable features
visible from several angles (hair, garment colours), and confirm by viewing frames.

## Video ranges

```sh
skywrite describe video interview.mp4 --start 56 --duration 8 -o laugh.json
```

The range is re-encoded to H.264 at 480p without audio (`--height` 240–1080) and
sent inline. The model reports shots, timed events (gestures, head turns,
expression changes, visible speech onsets) and people, with clip-relative times plus
`source_*` times. Ranges are limited to 600 seconds and 17 MiB encoded; shorter
ranges give finer timing. Results are cached by source content, range, encoding,
model, prompt version and cast. Descriptions are observations of a reduced clip:
confirm important moments with `review sheet`.
