use skywrite::media;
use std::path::Path;
use std::process::Command;
fn scratch() -> tempfile::TempDir {
    let root = Path::new("/home/hermes/.cache/skywrite-development");
    std::fs::create_dir_all(root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}
fn ffmpeg(args: &[&str], output: &Path) {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(args)
        .arg(output)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn tone(path: &Path, seconds: &str) {
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            &format!("sine=sample_rate=24000:duration={seconds}"),
            "-c:a",
            "pcm_s16le",
        ],
        path,
    );
}
#[test]
fn counted_samples_cache_source_change_and_estimate_range() {
    let d = scratch();
    let input = d.path().join("tone.wav");
    tone(&input, "0.3");
    let info = media::probe(&input).unwrap();
    let first = media::exact_audio_timing(&info, Some(d.path())).unwrap();
    assert_eq!(first.samples, 7200);
    assert!(!first.cache_hit);
    assert!((first.presentation_end_seconds - 0.3).abs() < 1e-6);
    assert!(
        media::exact_audio_timing(&info, Some(d.path()))
            .unwrap()
            .cache_hit
    );
    tone(&input, "0.6");
    let next = media::exact_audio_timing(&media::probe(&input).unwrap(), Some(d.path())).unwrap();
    assert!(!next.cache_hit);
    assert_eq!(next.samples, 14400);
    assert_ne!(first.source_sha256, next.source_sha256);
    let mut wrong = media::probe(&input).unwrap();
    wrong.duration = 50.;
    assert!(media::validate_range(&wrong, 0., 1.).is_err());
}
#[test]
fn positive_matroska_origin_and_delayed_audio_keep_presentation() {
    let d = scratch();
    let p = d.path().join("offset.mkv");
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "color=size=32x32:rate=10:duration=2",
            "-itsoffset",
            "0.5",
            "-f",
            "lavfi",
            "-i",
            "sine=sample_rate=24000:duration=1",
            "-c:v",
            "ffv1",
            "-c:a",
            "pcm_s16le",
            "-output_ts_offset",
            "5",
        ],
        &p,
    );
    let info = media::probe(&p).unwrap();
    assert!((info.duration - 2.).abs() < 1e-6);
    let exact = media::exact_audio_timing(&info, Some(d.path())).unwrap();
    assert_eq!(exact.samples, 24000);
    assert!((exact.decoded_duration_seconds - 1.).abs() < 1e-6);
    assert!((exact.presentation_end_seconds - 1.5).abs() < 1e-6);
    assert!((media::resolve_audio_only_timing(info).unwrap().duration - 2.).abs() < 1e-6);
}
#[test]
fn actual_provider_vbr_fixture_when_requested() {
    let Some(path) = std::env::var_os("SKYWRITE_REAL_VBR_FIXTURE") else {
        return;
    };
    let d = scratch();
    let info = media::probe(Path::new(&path)).unwrap();
    let timing = media::exact_audio_timing(&info, Some(d.path())).unwrap();
    assert!((info.duration - 15.563718).abs() < 0.001);
    assert_eq!(timing.samples, 228143);
    assert_eq!(timing.sample_rate, 24000);
    assert!((timing.presentation_end_seconds - 9.505958).abs() < 0.000002);
    assert!((timing.decoded_duration_seconds - 9.505958333).abs() < 1e-8);
}
#[test]
fn verify_cli_counts_checks_metadata_only_and_protects_input() {
    let d = scratch();
    let p = d.path().join("av.mkv");
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "color=size=32x32:rate=10:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=sample_rate=24000:duration=1",
            "-c:v",
            "ffv1",
            "-c:a",
            "pcm_s16le",
        ],
        &p,
    );
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_skywrite"))
            .arg("verify")
            .arg(&p)
            .args(extra)
            .output()
            .unwrap()
    };
    let out = run(&[
        "--expected-duration",
        "1",
        "--expected-width",
        "32",
        "--expected-height",
        "32",
        "--expected-fps",
        "10",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["video"]["frames"], 10);
    assert_eq!(value["audio"]["samples"], 24000);
    assert_eq!(value["full_decode_executed"], true);
    assert!(!value["audio"]["peak_dbfs"].is_null());
    let out = run(&["--expected-duration", "2"]);
    assert!(!out.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["passed"],
        false
    );
    let out = run(&["--metadata-only"]);
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["full_decode_executed"], false);
    assert!(value["audio"].is_null());
    assert!(value["video"].is_null());
    let hash = media::file_sha256(&p).unwrap();
    let out = run(&["--output", p.to_str().unwrap(), "--overwrite"]);
    assert!(!out.status.success());
    assert_eq!(hash, media::file_sha256(&p).unwrap());
    let report = d.path().join("report.json");
    std::fs::write(&report, b"keep").unwrap();
    assert!(
        !run(&["--output", report.to_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(std::fs::read(report).unwrap(), b"keep");
}
#[cfg(unix)]
#[test]
fn fast_inspect_never_invokes_ffmpeg() {
    use std::os::unix::fs::symlink;
    let d = scratch();
    let p = d.path().join("tone.wav");
    tone(&p, "0.1");
    let tools = d.path().join("tools");
    std::fs::create_dir(&tools).unwrap();
    let real = Command::new("which").arg("ffprobe").output().unwrap();
    symlink(
        String::from_utf8_lossy(&real.stdout).trim(),
        tools.join("ffprobe"),
    )
    .unwrap();
    let run = |exact: bool| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_skywrite"));
        c.env("PATH", &tools).arg("inspect").arg(&p);
        if exact {
            c.arg("--exact-audio").arg("--cache-dir").arg(d.path());
        }
        c.output().unwrap()
    };
    let out = run(false);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["media"][0]["duration_basis"], "metadata_estimate");
    assert!(!run(true).status.success());
}
#[test]
fn malformed_vbr_without_seek_header_uses_samples_not_bitrate() {
    let d = scratch();
    let p = d.path().join("vbr.mp3");
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            r"aevalsrc=if(lt(t\,1)\,0\,random(0)*0.1):s=24000:d=2",
            "-c:a",
            "libmp3lame",
            "-q:a",
            "2",
            "-write_xing",
            "0",
        ],
        &p,
    );
    let info = media::probe(&p).unwrap();
    let t = media::exact_audio_timing(&info, Some(d.path())).unwrap();
    assert!((info.duration - t.presentation_end_seconds).abs() > 0.5);
    assert_eq!(t.samples, 49536);
    assert!((t.presentation_end_seconds - 2.064).abs() < 0.000001);
}
