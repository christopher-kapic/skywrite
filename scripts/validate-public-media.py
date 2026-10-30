#!/usr/bin/env python3
"""Explicit public fixture preparation and optional paid CLI validation.

Default `check` is offline and never sets an API key. Media stays outside git.
No paid operation is retried. Existing successful outputs are reused.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import urllib.request


BASE = "https://groups.inf.ed.ac.uk/ami/AMICorpusMirror/amicorpus/ES2002a/"
MODEL = "google/gemini-3.5-flash-lite"
VIDEO_PROMPT = "Describe visible people, objects, and candidate editorial selects. Stay factual about sparsely sampled frames."
AUDIO_PROMPT = "Summarize the actual speech topics and recording quality; candidate selects must use source seconds."
BRIEF = (
    "Create exactly 8 seconds at 384x288 25fps: show the remote-control demonstration and listener. "
    "Use only video ranges0..30. Include an audio overlay from ami-array.wav source25..30 on "
    "timeline0..5 seconds about the kick-off meeting, with the remaining3 seconds silent. "
    "This is an editorial cutaway, do not imply measured lip synchronization. Respect all analyzed "
    "ranges and actual source durations. Return a JSON edit INSTANCE with only "
    "version,name,width,height,fps,clips,audio top-level keys. Do not copy schema metadata: "
    "omit $schema,title,type,properties,required,additionalProperties."
)
ATTRIBUTION = """# AMI corpus validation excerpts

Credit: AMI Consortium, AMI Meeting Corpus, meeting ES2002a.
Sources and license statement: https://groups.inf.ed.ac.uk/ami/download/
License: Creative Commons Attribution 4.0 International
https://creativecommons.org/licenses/by/4.0/
Headset-0, Array1-01, Mix-Headset audio: source30..150s, mono16kHz PCM.
Closeup1 video: source30..60s, resized384x288 H264, no audio.
Derived audio: asetrate16160/16002, resample16000, prepend2/1s silence.
These changes and resulting edits are derivative excerpts; retain attribution.
"""


def execute(command, directory, name, env=None):
    start = time.monotonic()
    result = subprocess.run(command, capture_output=True, text=True, env=env, timeout=240)
    (directory / (name + ".stdout.json")).write_text(result.stdout)
    (directory / (name + ".stderr")).write_text(result.stderr)
    record = {"command": command, "exit": result.returncode,
              "runtime_seconds": time.monotonic() - start}
    (directory / (name + ".run.json")).write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps(record), flush=True)
    if result.returncode:
        raise RuntimeError(result.stderr.strip())
    return json.loads(result.stdout) if result.stdout.strip() else None


def prepare(p):
    sources = []
    for stem, remote, seconds in [
        ("ami-headset.wav", "audio/ES2002a.Headset-0.wav", 120),
        ("ami-array.wav", "audio/ES2002a.Array1-01.wav", 120),
        ("ami-mix.wav", "audio/ES2002a.Mix-Headset.wav", 120),
        ("ami-video.mp4", "video/ES2002a.Closeup1.avi", 30),
    ]:
        source = BASE + remote
        output = p / stem
        sources.append({"source": source, "derived": stem, "source_start_seconds": 30})
        if output.exists():
            continue
        command = ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error",
                   "-ss", "30", "-i", source, "-t", str(seconds)]
        if output.suffix == ".mp4":
            command += ["-an", "-vf", "scale=384:288", "-c:v", "libx264", "-threads", "1", "-preset", "fast"]
        else:
            command += ["-ac", "1", "-ar", "16000"]
        execute(command + [str(output)], p, "prepare-" + output.stem)
    for stem, rate, delay in [("speed101-delay2", 16160, 2000), ("drift125ppm-delay1", 16002, 1000)]:
        output = p / ("ami-array-" + stem + ".wav")
        if not output.exists():
            execute(["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error",
                     "-i", str(p / "ami-array.wav"), "-af",
                     f"asetrate={rate},aresample=16000,adelay={delay}:all=1",
                     "-ac", "1", "-ar", "16000", str(output)], p, "prepare-" + stem)
    (p / "ATTRIBUTION.md").write_text(ATTRIBUTION + "\n" + "\n".join(x["source"] for x in sources) + "\n")
    for item in sources:
        item["sha256"] = hashlib.sha256((p / item["derived"]).read_bytes()).hexdigest()
    (p / "fixtures.json").write_text(json.dumps(sources, indent=2) + "\n")


def check(p, binary):
    env = os.environ.copy()
    env.pop("OPENROUTER_API_KEY", None)
    rows = []
    # Acoustic propagation and approximate source synchronization allow 20ms/250ppm.
    for name, asset, offset, scale in [
        ("mics", "ami-array.wav", 0, 1),
        ("speed101-delay2", "ami-array-speed101-delay2.wav", -2.02, 1.01),
        ("drift125ppm-delay1", "ami-array-drift125ppm-delay1.wav", -1.000125, 1.000125),
    ]:
        result = execute([binary, "sync", "--reference", str(p / "ami-headset.wav"),
                          "--asset", str(p / asset), "--method", "audio"], p, "check-sync-" + name, env)
        offset_error = abs(result["offset_seconds"] - offset)
        scale_error = abs(result["scale"] - scale) * 1e6
        assert offset_error < 0.02, (name, "offset error", offset_error)
        assert scale_error < 250, (name, "scale error ppm", scale_error)
        assert result["inliers"] >= 8 and result["anchor_span_seconds"] > 60, name
        rows.append({"case": name, "offset_error_seconds": offset_error,
                     "scale_error_ppm": scale_error, "quality": result["quality"]})
    if (p / "suggested-edit.json").exists():
        execute([binary, "edit", "validate", str(p / "suggested-edit.json")], p, "check-edit", env)
    if (p / "mix-transcript.json").exists():
        transcript = execute([binary, "transcribe", str(p / "ami-mix.wav"), "--start", "25",
                              "--duration", "30", "--language", "en", "--cache-dir", str(p / "cache")],
                             p, "check-transcript-cache", env)
        assert transcript["usage"]["cache_hits"] == 1
        assert "project manager" in transcript["text"].lower()
    for filename in ["video-analysis.json", "audio-analysis.json"]:
        if not (p / filename).exists():
            continue
        analysis = json.loads((p / filename).read_text())
        start = analysis["range"]["start"]
        end = start + analysis["range"]["duration"]
        understanding = analysis["understanding"]
        for observation in understanding["observations"]:
            assert start <= observation["time"] <= end
        for select in understanding["selects"]:
            assert start <= select["source_in"] < select["source_out"] <= end
    (p / "offline-checks.json").write_text(json.dumps(rows, indent=2) + "\n")


def key_status(key, destination):
    request = urllib.request.Request("https://openrouter.ai/api/v1/auth/key",
                                    headers={"Authorization": "Bearer " + key})
    with urllib.request.urlopen(request, timeout=30) as response:
        data = json.load(response)["data"]
    # Store accounting only, never credential label, account identifiers, or token.
    safe = {k: data.get(k) for k in ["usage", "usage_daily", "limit", "limit_remaining", "expires_at"]}
    destination.write_text(json.dumps(safe, indent=2) + "\n")
    return float(data["usage"])


def live(p, binary, args):
    if not args.allow_paid:
        raise RuntimeError("live requires explicit --allow-paid; offline check makes no requests")
    key = Path(args.key_file).read_text().strip() if args.key_file else os.environ.get("OPENROUTER_API_KEY", "")
    if not key:
        raise RuntimeError("set OPENROUTER_API_KEY or provide --key-file (never committed)")
    env = os.environ.copy()
    env["OPENROUTER_API_KEY"] = key
    before = key_status(key, p / "script-key-before.json")
    jobs = [
        ("mix-transcript.json", ["transcribe", str(p / "ami-mix.wav"), "--start", "25", "--duration", "30", "--language", "en"]),
        ("video-analysis.json", ["analyze", str(p / "ami-video.mp4"), "--frames", "4", "--model", args.model,
                                 "--prompt", VIDEO_PROMPT, "--frames-dir", str(p / "frames")]),
        ("audio-analysis.json", ["analyze", str(p / "ami-array.wav"), "--duration", "30", "--model", args.model,
                                 "--prompt", AUDIO_PROMPT, "--frames-dir", str(p / "frames")]),
        ("suggested-edit.json", ["edit", "suggest", "--analysis", str(p / "video-analysis.json"),
                                 str(p / "audio-analysis.json"), "--brief", BRIEF, "--model", args.model]),
    ]
    try:
        for index, (filename, command) in enumerate(jobs):
            if (p / filename).exists():
                print("Reusing existing output: " + filename)
                continue
            spent = key_status(key, p / "script-key-current.json") - before
            if spent >= args.paid_budget:
                raise RuntimeError("local paid budget exhausted; no further requests")
            execute([binary] + command + ["--cache-dir", str(p / "cache"), "-o", str(p / filename)],
                    p, "live-" + str(index), env)
        transcript = json.loads((p / "mix-transcript.json").read_text())
        assert len(transcript["words"]) > 40 and "project manager" in transcript["text"].lower()
        assert min(w["start"] for w in transcript["words"]) >= 25
        assert max(w["end"] for w in transcript["words"]) <= 55.15
        execute([binary, "edit", "validate", str(p / "suggested-edit.json")], p, "live-edit-validate", env)
        if not (p / "suggested-edit.mp4").exists():
            execute([binary, "edit", "render", str(p / "suggested-edit.json"), "-o", str(p / "suggested-edit.mp4")], p, "live-render", env)
        probe = execute(["ffprobe", "-v", "error", "-show_streams", "-show_format", "-of", "json",
                         str(p / "suggested-edit.mp4")], p, "live-render-probe", env)
        assert abs(float(probe["format"]["duration"]) - 8) < 0.05
        assert {s["codec_type"] for s in probe["streams"]} >= {"audio", "video"}
    finally:
        after = key_status(key, p / "script-key-after.json")
        print(json.dumps({"spend_before_usd": before, "spend_after_usd": after, "delta_usd": after - before}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", nargs="?", choices=["check", "prepare", "live"], default="check")
    parser.add_argument("--directory", type=Path, default=Path("/tmp/skywrite-public-validation"))
    parser.add_argument("--binary", default="target/debug/skywrite")
    parser.add_argument("--allow-paid", action="store_true")
    parser.add_argument("--paid-budget", type=float, default=0.25,
                        help="stop before a next request once this run's observed cost reaches budget; use a server key cap too")
    parser.add_argument("--key-file")
    parser.add_argument("--model", default=MODEL)
    args = parser.parse_args()
    if not 0 < args.paid_budget <= 1:
        parser.error("paid-budget must be >0 and <=1 USD")
    p = args.directory.resolve()
    p.mkdir(parents=True, exist_ok=True)
    if args.mode == "prepare":
        prepare(p)
    elif args.mode == "live":
        live(p, str(Path(args.binary).resolve()), args)
    else:
        check(p, str(Path(args.binary).resolve()))


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, RuntimeError, subprocess.TimeoutExpired, OSError, ValueError) as error:
        print("Validation failed: " + str(error), file=sys.stderr)
        sys.exit(1)
