# Local technical verification

`skywrite inspect FILE` reads headers through ffprobe. Its duration is labeled
`metadata_estimate`; variable bitrate MP3 headers can overstate actual playtime.
`skywrite inspect FILE --exact-audio` decodes the first audio stream to a null
sink and reports counted samples, sample rate, decoded sample duration, and the
last audio presentation end relative to the source origin. For audio-only media,
`duration` then uses that measured presentation end. Video duration remains the
metadata estimate; audio timing never truncates a video tail.

Exact timing is cached under the normal Skywrite cache root (override with
`--cache-dir` or `SKYWRITE_CACHE_DIR`), keyed by source SHA-256, timing-policy
version and the installed FFmpeg build. Content changes require fresh decoding.
`cache_hit` distinguishes reused evidence. Sample duration excludes timestamp gaps;
presentation end includes source-relative delay and gaps. The decoder handles
codec priming. The cache is regeneratable local evidence, not a signed attestation.
Existing MediaInfo JSON remains readable; inspection adds explanatory fields.

`skywrite verify FILE --expected-duration 10 --duration-tolerance 0.05
--expected-width 1280 --expected-height 720 --expected-fps 30` performs a fresh
complete decode of the first audio and video streams. It reports audio sample
count, peak/RMS in dBFS, video frame count and decoded presentation end. Silence
has null peak/RMS, representing negative infinity. It checks requested expectations,
writes JSON to stdout, and exits nonzero for failed expectations. Use
`--output report.json` to also save it; replacement requires `--overwrite`.
An input cannot be its own report output, including with `--overwrite`.

`--metadata-only` skips decoding and labels duration evidence as estimated.
Width, height and average FPS checks use stream metadata; average FPS does not
establish constant frame rate. No audio levels, decoded counts or measured
duration are claimed in this mode.

Each decode has a 600-second wall deadline, six-hour presentation/sample work
limit, one MiB cap on each diagnostic/progress pipe and 16 GiB input limit. PCM is
never buffered or written to disk. Over-limit, corrupt, empty or incomplete
inputs fail without publishing exact evidence. Cache lock waiting is bounded.
Timing-sensitive audio-only range validation, transcription, timeline sources,
generation validation and audio synchronization use decoded timing automatically.
No command here accesses the network. Verification reports technical facts only;
it does not establish artistic quality, suitability for children or compatibility
with a particular editor.

`skywrite edit suggest --analysis analysis.json --transcript words.json --brief ...`
attaches existing provider word timestamps without another transcription or
analysis request. Repeat `--transcript` for additional sources. Analysis and
transcript files share an eight MiB limit on bytes actually read, admitted before
JSON parsing. Sources must bind to analyzed media; output protection includes
supplied transcripts and their media. Audible cuts inside supplied words produce
advisory stderr warnings; intentional B-roll and J/L cuts remain valid. The saved
result remains Edit v1. The requested editing-model call occurs only after local
input admission and source checks.
