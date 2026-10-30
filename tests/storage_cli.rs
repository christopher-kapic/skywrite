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

#[test]
fn local_job_scan_fails_closed_before_cloud_and_cleanup() {
    use base64::Engine;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    success(cli(
        home,
        &[
            "storage",
            "configure",
            "--endpoint",
            "https://example.com",
            "--bucket",
            "test-bucket",
            "--public-base-url",
            "https://assets.example.com",
        ],
    ));
    let purge = |dry_run: bool| {
        let args = if dry_run {
            vec!["storage", "purge", "--dry-run"]
        } else {
            vec!["storage", "purge"]
        };
        let out = cli(home, &args);
        assert!(!out.status.success());
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let allowed = || {
        assert!(
            purge(true).contains("AWS_ACCESS_KEY_ID"),
            "legitimate absence must reach credential validation"
        )
    };
    let blocked = |expected: &str| {
        for dry_run in [true, false] {
            let error = purge(dry_run);
            assert!(error.contains(expected), "expected {expected}: {error}");
            assert!(
                !error.contains("AWS_ACCESS_KEY_ID"),
                "guard must precede cloud creation: {error}"
            );
        }
    };
    allowed(); // Genuinely missing jobs root.
    let jobs = home.join("jobs");
    let other = jobs.join("a".repeat(64));
    std::fs::create_dir_all(&other).unwrap();
    std::fs::create_dir_all(jobs.join("unrelated")).unwrap();
    std::fs::write(jobs.join("b".repeat(64)), "not a job directory").unwrap();
    allowed(); // Empty valid-ID and unrelated directories are not job records.
    let record = other.join("job.json");
    std::fs::write(&record, "{}").unwrap();
    blocked("missing field");
    let override_result = cli(home, &["storage", "purge", "--dry-run", "--include-active"]);
    assert!(String::from_utf8_lossy(&override_result.stderr).contains("AWS_ACCESS_KEY_ID"));

    let config: Value =
        serde_json::from_slice(&std::fs::read(home.join("storage.json")).unwrap()).unwrap();
    let hash = "c".repeat(64);
    let upload = json!({"config":config,"key":format!("skywrite/uploads/{hash}.mp4"),"sha256":hash,"bytes":1,"media_type":"video/mp4"});
    let make_job = |id: &str, status: &str, artifact: Value| json!({"version":1,"id":id,"task":"image-generation","model":"test/model","status":status,"created_at":1,"updated_at":1,"request":{},"references":[{"storage_upload":upload,"cleanup_requested":true}],"remote_id":null,"generation_id":null,"usage":null,"artifact":artifact,"error":null});
    let id = "d".repeat(64);
    let completed = jobs.join(&id);
    std::fs::create_dir(&completed).unwrap();
    let bytes = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jQZkAAAAASUVORK5CYII=").unwrap();
    let result = completed.join("result.png");
    std::fs::write(&result, &bytes).unwrap();
    let artifact = json!({"file":"result.png","sha256":skywrite::media::file_sha256(&result).unwrap(),"bytes":bytes.len(),"media_type":"image/png","decode_validation_version":1});
    let completed_job = make_job(&id, "completed", artifact.clone());
    let resume = || {
        std::fs::write(
            completed.join("job.json"),
            serde_json::to_vec(&completed_job).unwrap(),
        )
        .unwrap();
        success(cli(home, &["jobs", "resume", &id]))
    };
    let failed = resume();
    assert_eq!(failed["references"][0]["cleanup_status"], "failed");
    assert!(
        failed["references"][0]["cleanup_error"]
            .as_str()
            .unwrap()
            .contains("missing field")
    );

    // Exercise the shared deletion boundary's inverse: every unfinished state
    // retains the source; terminal records allow credential validation to proceed.
    for state in [
        "prepared",
        "submitting",
        "uncertain",
        "received",
        "pending",
        "in_progress",
        "ready",
        "failed",
        "rejected",
        "completed",
    ] {
        let mut job = make_job(&"a".repeat(64), state, artifact.clone());
        job["remote_id"] = json!("provider-id");
        std::fs::write(&record, serde_json::to_vec(&job).unwrap()).unwrap();
        let value = resume();
        let reference = &value["references"][0];
        if matches!(state, "failed" | "rejected" | "completed") {
            assert_eq!(reference["cleanup_status"], "failed", "{state}");
            assert!(
                reference["cleanup_error"]
                    .as_str()
                    .unwrap()
                    .contains("AWS_ACCESS_KEY_ID"),
                "{state}: {reference}"
            );
        } else {
            assert_eq!(
                reference["cleanup_status"], "deferred_active_job",
                "{state}"
            );
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        std::fs::remove_file(&record).unwrap();
        symlink(other.join("missing-record"), &record).unwrap();
        blocked("opening"); // A dangling record is existing invalid state.
        std::fs::remove_file(&record).unwrap();
        std::fs::remove_dir(&other).unwrap();
        symlink(jobs.join("unrelated"), &other).unwrap();
        blocked("symlink generation job directory");
        std::fs::remove_file(&other).unwrap();
        let archived = home.join("archived-jobs");
        std::fs::rename(&jobs, &archived).unwrap();
        symlink(&archived, &jobs).unwrap();
        blocked("jobs root must be a non-symlink directory");
        std::fs::remove_file(&jobs).unwrap();
        std::fs::write(&jobs, "not a directory").unwrap();
        blocked("jobs root must be a non-symlink directory");
        std::fs::remove_file(&jobs).unwrap();
        std::fs::rename(&archived, &jobs).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::fs::write(&record, "{}").unwrap();
        // Root bypasses Unix DAC, so chmod cannot create a permission fault there.
        if unsafe { libc::geteuid() } != 0 {
            for path in [&record, &other, &jobs] {
                let original = std::fs::metadata(path).unwrap().permissions();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o0)).unwrap();
                blocked("Permission denied");
                if path != &jobs {
                    let value = resume();
                    assert_eq!(value["references"][0]["cleanup_status"], "failed");
                    let error = value["references"][0]["cleanup_error"].as_str().unwrap();
                    assert!(error.contains("Permission denied"), "{error}");
                    assert!(!error.contains("AWS_ACCESS_KEY_ID"), "{error}");
                }
                std::fs::set_permissions(path, original).unwrap();
            }
        } else {
            eprintln!("permission-only cases skipped: root bypasses Unix DAC");
        }
    }
}
