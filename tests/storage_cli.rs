//! Local-only contract checks. These do not establish S3/R2 compatibility.
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Output},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
fn cli(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(args)
        .env("SKYWRITE_HOME", home)
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .env_remove("OPENROUTER_API_KEY")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .output()
        .unwrap()
}
fn success(out: Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
fn catalog(root: &Path, endpoint: &str, data: Value) {
    let key = skywrite::openrouter::cache_key(&[b"catalog-v1", endpoint.as_bytes()]);
    let body = json!({"version":1,"endpoint":endpoint,"fetched_at":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),"body":{"data":data}});
    skywrite::io::write_json(
        &root.join("models").join(format!("{key}.json")),
        &body,
        false,
    )
    .unwrap();
}
#[test]
fn local_config_and_upload_plans_never_need_cloud_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let config = success(cli(
        &home,
        &[
            "storage",
            "configure",
            "--endpoint",
            "https://example.r2.cloudflarestorage.com",
            "--bucket",
            "test-bucket",
            "--public-base-url",
            "https://assets.example.com",
            "--prefix",
            "skywrite/uploads/test-project/",
        ],
    ));
    assert_eq!(config["remote_check_performed"], false);
    assert!(config.get("live_bucket_verified").is_none());
    assert_eq!(
        success(cli(&home, &["storage", "status"]))["configured"],
        true
    );
    let configbytes = std::fs::read(home.join("storage.json")).unwrap();
    assert!(!String::from_utf8_lossy(&configbytes).contains("secret"));
    let source = temp.path().join("source.mp4");
    let mut ff = skywrite::process::args(&[
        "-v",
        "error",
        "-nostdin",
        "-f",
        "lavfi",
        "-i",
        "color=blue:s=16x16:r=10:d=0.3",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
    ]);
    ff.push(source.as_os_str().to_owned());
    skywrite::process::run("ffmpeg", &ff, Duration::from_secs(15), 1024 * 1024).unwrap();
    let sha = skywrite::media::file_sha256(&source).unwrap();
    let plan = success(cli(
        &home,
        &["storage", "upload", source.to_str().unwrap(), "--dry-run"],
    ));
    assert_eq!(plan["cloud_calls"], false);
    assert_eq!(plan["upload"]["sha256"], sha);
    assert_eq!(
        plan["upload"]["key"],
        format!("skywrite/uploads/test-project/{sha}.mp4")
    );
    assert!(!home.join("jobs").exists());
    let cache = temp.path().join("cache");
    catalog(
        &cache,
        "videos/models",
        json!([{"id":"vendor/upscale","upscale_factor":{"min":1.5,"max":3},"creativity":[0,1]}]),
    );
    catalog(&cache, "models?output_modalities=video", json!([]));
    let args = vec![
        "upscale",
        "video",
        source.to_str().unwrap(),
        "--upload",
        "--delete-upload-after",
        "--model",
        "vendor/upscale",
        "--cache-dir",
        cache.to_str().unwrap(),
        "--offline",
        "--dry-run",
    ];
    let generation = success(cli(&home, &args));
    assert_eq!(generation["dry_run"], true);
    assert_eq!(generation["paid_request"], false);
    assert_eq!(generation["references"][0]["storage_upload"]["sha256"], sha);
    assert_eq!(generation["references"][0]["cleanup_requested"], true);
    assert!(!home.join("jobs").exists());
    let copied = temp.path().join("copied.mp4");
    std::fs::copy(&source, &copied).unwrap();
    let mut copyargs = args.clone();
    copyargs[2] = copied.to_str().unwrap();
    assert_eq!(success(cli(&home, &copyargs))["id"], generation["id"]);
    let configpath = home.join("storage.json");
    let mut bad = args.clone();
    bad.extend(["--output", configpath.to_str().unwrap(), "--overwrite"]);
    let rejection = cli(&home, &bad);
    assert!(!rejection.status.success());
    assert!(String::from_utf8_lossy(&rejection.stderr).contains("overwrite an input file"));
    assert_eq!(std::fs::read(configpath).unwrap(), configbytes);
    let onlyflag = cli(
        &home,
        &[
            "upscale",
            "video",
            source.to_str().unwrap(),
            "--delete-upload-after",
        ],
    );
    assert!(!onlyflag.status.success());
}
