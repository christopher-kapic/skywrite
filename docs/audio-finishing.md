# Local audio finishing

`skywrite audio` uses installed FFmpeg/ffprobe and makes no model calls. Commands emit JSON with source hashes, first-audio decoded timing, sample rounding, format and actual final decoded duration. Outputs are 48-kHz stereo PCM16 WAV or 192-kbit/s MP3, selected by `.wav`/`.mp3`; bytes are encoded and checked, not renamed.

```sh
# Normalize speech with two-pass integrated loudness and a true-peak ceiling.
skywrite audio normalize narration.mp3 -o narration.wav --lufs -18 --peak-db -2
# Or normalize sample peak alone, without a LUFS target.
skywrite audio normalize narration.wav -o peak.wav --peak-db -6
# A multiplier above one speeds up speech, preserving pitch.
skywrite audio tempo narration.wav -o faster.wav --tempo 1.1
# Repeat one cue with overlapping crossfades, then fade to an exact endpoint.
skywrite audio loop cue.wav -o bed.wav --duration 180 --crossfade 2 \
  --lufs -24 --peak-db -4 --fade-in 1.5 --fade-out 5
# Narration controls attenuation. The output contains only the ducked music.
skywrite audio duck bed.wav -o ducked.wav --sidechain narration-stem.wav \
  --threshold 0.035 --ratio 4 --attack-ms 15 --release-ms 600 --duration 180
# One composable command can trim, adjust speed/levels, fade and pad.
skywrite audio finish speech.mp3 -o finished.wav --start 0.1 --length 12 \
  --tempo 1.05 --lufs -18 --peak-db -2 --fade-in 0.025 --duration 14
```

Every command accepts the same typed options. `normalize` defaults to -18 LUFS when neither level target is supplied; `tempo` requires `--tempo`, `loop` requires `--duration`, and `duck` requires `--sidechain`. `finish` applies only the requested operations. Use `finish --loop --duration ...` to compose looping with other steps. `--crossfade 0` uses direct concatenation. Crossfades must be shorter than half the selected, tempo-adjusted loop; each overlap reduces the repeat stride. A target shorter than the cue trims it without repeating. This first version repeats one cue; assemble different cues on edit tracks or prepare their sequence separately.

Processing order is source selection, pitch-preserving tempo, normalization, loop/crossfade, narration-driven ducking, final padding/trim, then fades. Normalization targets the prepared selection. Later crossfades, ducking, fades and MP3 encoding can change final loudness or peaks; the report does not claim a final LUFS target was verified. Two-pass loudnorm may use its dynamic fallback if linear normalization cannot satisfy the requested range/ceiling. Digital silence stays silent and reports that the level target is unachievable. Peak-only normalization targets decoded sample peak, rather than intersample true peak.

`--start` and `--length` refer to decoded samples of the first audio stream, starting at its first sample, irrespective of container timestamp origins or gaps. The output timeline starts at zero. The JSON retains container/stream start timestamps and the original source hash: preserve these when mapping back to source presentation time. At constant tempo, source sample time equals the reported selected start plus output time multiplied by tempo; loops repeat that mapping. Narration's first decoded sample aligns to output zero, with no implicit offset. To duck a film, supply a narration stem with its deliberate timeline silence already included. Short narration is padded with silence so the music continues and recovers.

Source selection, final duration and fades round to the nearest 48-kHz sample. Tempo output duration follows the actual decoder/filter output, not a promise of exact arithmetic division. `--duration` always pads silence and then trims to the requested sample count, even if a previous filter produced a short tail (for example 179.2 seconds for a 180-second film). A fresh full decode of the encoded output must match that sample count before publication. MP3's container duration may include encoder delay/padding; the decoded gapless sample count is the reported endpoint. Prefer WAV for timeline stems.

Existing outputs require `--overwrite`. Every consumed input, including the sidechain, is protected against output replacement; original media is never edited. Processing uses owned temporary files beside the output and atomically publishes only a complete, freshly decoded result. Failure or ordinary cancellation preserves any previous output. No paths or user text enter a shell or raw filter graph. Paths beginning with a dash work as positional arguments after `--`, or as absolute paths.

Resource limits are one GiB per source/output, one hour of decoded source/final audio, eight input channels, 192-kHz input, 128 loop repetitions, tempo 0.25–4, crossfades 0–30 seconds and finite validated numeric parameters. Processing stages share a 15-minute deadline, each supervised FFmpeg process is capped at ten minutes and one MiB diagnostic output. Exact timing uses the existing bounded six-hour/ten-minute decoder and bounded cache-lock waits; its wall time is additional to processing stages. No decoded PCM is captured on stdout. Temporary disk usage can include several WAV intermediates (up to several GiB at the maximum duration).

For an existing Pip-style film, finish each voice section, assemble voice placements using edit audio offsets, then loop a music cue and duck it against the assembled narration stem. Keep the measured per-section durations with the edit instead of inferring them from MP3 metadata. See [audio generation](audio-generation.md) and [edit format](edit-format.md).
