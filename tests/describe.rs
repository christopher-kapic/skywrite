//! Frame descriptions are cached per exact frame and need no key when cached.
use serde_json::{Value, json};
use std::process::Command;

fn run(args: &[&str], cache: &std::path::Path, home: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .env_remove("OPENROUTER_API_KEY")
        .env("SKYWRITE_HOME", home)
        .args(args)
        .arg("--cache-dir")
        .arg(cache)
        .output()
        .unwrap()
}

#[test]
fn seeded_frame_descriptions_are_reused_across_overlapping_passes() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("v.mp4");
    assert!(
        Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=s=160x90:r=30:d=3",
                "-pix_fmt",
                "yuv420p"
            ])
            .arg(&video)
            .status()
            .unwrap()
            .success()
    );
    let cache = dir.path().join("cache");
    let home = dir.path().join("home");
    let sha = skywrite::media::file_sha256(&video).unwrap();
    let model = "fixture/frames";
    // Seed one second-spaced frames: 0, 1, 2 seconds.
    for second in 0..3 {
        let key = skywrite::describe::frame_cache_key(&sha, second as f64, model, None).unwrap();
        let path = cache.join("frame-descriptions").join(format!("{key}.json"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"version":1,"model":model,
            "description":{"summary":format!("second {second}")},"usage":{"cost":0.001}}))
            .unwrap(),
        )
        .unwrap();
    }
    let source = video.to_str().unwrap();
    let out = run(
        &[
            "describe", "frames", source, "--every", "30", "--model", model,
        ],
        &cache,
        &home,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["counts"]["cached"], 3);
    assert_eq!(report["cost_usd"], 0.0);
    assert_eq!(report["frames"][1]["description"]["summary"], "second 1");
    assert_eq!(report["frames"][1]["frame"], 30);
    assert!(std::path::Path::new(report["frames"][1]["image"].as_str().unwrap()).is_file());

    // A dense pass around 1s shares the seeded frame at exactly 1.0s.
    let dry = run(
        &[
            "describe",
            "frames",
            source,
            "--around",
            "1",
            "--window",
            "0.2",
            "--model",
            model,
            "--dry-run",
        ],
        &cache,
        &home,
    );
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let dry: Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(dry["frames"], 3);
    assert_eq!(dry["cached"], 1);
    assert_eq!(dry["requests"], 0);

    // Uncached frames need a key, and the frame budget is enforced first.
    let budget = run(
        &[
            "describe",
            "frames",
            source,
            "--every",
            "2",
            "--model",
            model,
            "--max-frames",
            "5",
        ],
        &cache,
        &home,
    );
    assert!(!budget.status.success());
    assert!(String::from_utf8_lossy(&budget.stderr).contains("--max-frames"));

    // A cast changes the cache identity.
    let cast = dir.path().join("cast.json");
    std::fs::write(
        &cast,
        r#"{"version":1,"people":[{"name":"Host","description":"red jacket"}]}"#,
    )
    .unwrap();
    let with_cast = run(
        &[
            "describe",
            "frames",
            source,
            "--every",
            "30",
            "--model",
            model,
            "--cast",
            cast.to_str().unwrap(),
            "--dry-run",
        ],
        &cache,
        &home,
    );
    let with_cast: Value = serde_json::from_slice(&with_cast.stdout).unwrap();
    assert_eq!(with_cast["cached"], 0);
    std::fs::write(
        &cast,
        r#"{"version":1,"people":[{"name":"unknown","description":"x"}]}"#,
    )
    .unwrap();
    let invalid = run(
        &[
            "describe",
            "frames",
            source,
            "--every",
            "30",
            "--model",
            model,
            "--cast",
            cast.to_str().unwrap(),
            "--dry-run",
        ],
        &cache,
        &home,
    );
    assert!(!invalid.status.success());
}
