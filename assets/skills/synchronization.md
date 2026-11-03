# Align independent recordings

1. Inspect reference and asset files, preserving both originals. Choose a stable reference clock.
2. Run `skywrite sync --reference reference.wav --asset camera.mp4 --method auto`. Audio alignment is local; explicit transcription is a separate model command.
3. Review the confidence, ambiguity, anchors, drift, and analyzed-span diagnostics. Silence, repetitions, short overlap, and long unexamined tails need manual review; do not infer certainty from a returned offset alone.
4. The reported mapping is `reference_time = offset + scale * asset_time`. Place the asset on the reference timeline using that mapping; conform its playback with `speed = 1 / scale`.
5. If needed, produce transcripts explicitly with `skywrite transcribe INPUT` and consult `skywrite sync --help` for transcript input options and supported controls.
6. Write and validate the edit, render a review copy, and listen near the start, middle, and end for drift and double audio. Mute camera audio when using the independent recorder.

Source trims, timeline placement, and playback speed have different meanings. Never apply an offset as a source trim without deriving the desired overlap first.


## Dialogue cuts from real words

Use an existing source-bound transcript whenever available. `skywrite analyze source.mp4 --local --transcript transcript.json --start 10 --duration 20 -o analysis.json` performs local analysis without an ASR call. `edit suggest` supplies only original words overlapping the analyzed range and rejects changed source fingerprints. For a direct inspection of a bounded source window:

```sh
jq --argjson start 10 --argjson end 30 '[.words[] | select(.start < $end and (.end > $start or .start == $start))]' transcript.json
```

Keep words crossing either boundary; do not clamp their timestamps to the queried range. Choose complete thoughts inside the observed range or request a wider range. Use original starts/ends in playback seconds, including fractional precision. Container timecode and uniformly sampled frame seeks are separate concepts and cannot replace word alignment. A transcript is provider alignment evidence; listen around a cut before accepting it. An untranscribed interval is not proof of silence.

For audible speech, preserve the entire word span. `source_in` supports subframe precision; clip `start` and output `duration` follow the sequence frame grid. At speed `s`, source out is `source_in + duration*s`; round output duration outward when necessary to retain the last word. Do not round a word's start upward or end downward and call the remaining syllable a complete word.

Muted B-roll can change picture during a spoken word. Intentional J/L cuts can carry a complete dialogue span on a separate audio overlay while the picture changes. Do not enforce a blanket ban on visual midword cuts. No tool should silently transcribe, regenerate narration, or submit a paid retake to solve a cut.

To attach existing words directly to saved analyses, use `skywrite edit suggest --analysis analysis.json --transcript transcript.json --brief "Intent" --model MODEL`. Repeat `--transcript` for multiple sources. Source fingerprints must match an analyzed source; legacy transcripts require its canonical path and retain an unverified content-identity label. Identical duplicate word evidence is accepted; conflicting alignments fail rather than silently overriding an embedded transcript. The command never transcribes implicitly. Advisory warnings identify audible cuts inside supplied words; muted B-roll and continuous matching audio across cuts are exempt.
