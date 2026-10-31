# Captions from existing word timestamps

Convert an existing Skywrite transcript without transcription or network calls:

```sh
skywrite captions convert transcript.json --output captions.srt
skywrite captions convert transcript.json --output captions.vtt --format vtt \
  --offset 1.5 --scale 1.001 --max-words 8 --max-duration 4 --max-chars 80
```

The mapping is `caption_time = offset + scale * word_time`. Actual provider word
start/end timestamps determine cue boundaries. No segment timing is expanded into
invented word timing. Grouping joins at most 8 words, 4 seconds, and 80 characters
by default, and starts a new cue after a gap above one second. A single word that
exceeds a selected grouping limit is an error; increase the limit explicitly.
Negative/out-of-range mapped times are errors, with no silent clipping.

SRT and VTT output is UTF-8 with millisecond timestamps and escaped plain text.
Native rendering, generated Python, and the mux helper use WebVTT internally so
literal markup such as `<b>hello</b>` stays visible. Braces (`{`, `}`) and
backslashes are rejected before conversion/export/rendering: FFmpeg's subtitle
conversion cannot preserve these characters identically in soft tracks and
burn-in. Use ordinary newlines for line breaks. Unicode and quotes are preserved.
The exported SRT helper is for plain interchange; use VTT for FFmpeg playback
because its SRT decoder does not decode escaped markup entities.
Existing files require `--overwrite`; original transcript and referenced media
paths remain protected. The helper publishes only complete output and reports
JSON including cue count, explicit mapping, and `transcription_requested: false`.

Attach output cues and chapters through the optional edit version 1 fields in
[edit-format.md](edit-format.md#optional-captions-and-chapters). Tracks default to
soft MP4 subtitles. Burning into video always requires `mode: "burn"`.
