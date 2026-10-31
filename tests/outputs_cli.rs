use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};
fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .env("SKYWRITE_HOME", home)
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn output_discovery_cli_preflights_without_cloud_or_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let configured = run(
        &home,
        &[
            "storage",
            "configure",
            "--endpoint",
            "https://s3.example.invalid",
            "--bucket",
            "outputs-demo",
            "--public-base-url",
            "https://cdn.example.invalid",
        ],
    );
    assert!(
        configured.status.success(),
        "{}",
        String::from_utf8_lossy(&configured.stderr)
    );
    let source = dir.path().join("finished.json");
    std::fs::write(&source, b"{\"result\":true}").unwrap();
    let output = run(
        &home,
        &[
            "outputs",
            "upload",
            source.to_str().unwrap(),
            "--name",
            "diagram 🦊.json",
            "--dry-run",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["cloud_calls"], false);
    assert_eq!(value["output"]["name"], "diagram 🦊.json");
    assert_eq!(value["public_access_verified"], false);
    assert_eq!(
        value["public_url"].as_str().unwrap(),
        format!(
            "https://cdn.example.invalid/{}",
            value["output"]["blob_key"].as_str().unwrap()
        )
    );
    assert!(value["output"].get("public_url").is_none());
    assert!(
        value["output"]["blob_key"]
            .as_str()
            .unwrap()
            .starts_with("skywrite/outputs/blobs/")
    );
    let config = home.join("storage.json");
    let bytes = std::fs::read(&config).unwrap();
    let bad = run(
        &home,
        &[
            "outputs",
            "download",
            "friendly.mp4",
            config.to_str().unwrap(),
            "--overwrite",
        ],
    );
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("overwrite an input"));
    assert_eq!(std::fs::read(&config).unwrap(), bytes);
    let destination = dir.path().join("keep.mp4");
    std::fs::write(&destination, b"keep").unwrap();
    for args in [
        vec!["outputs", "url", "../escape"],
        vec!["storage", "check-public", "../escape"],
        vec!["outputs", "list", "--limit", "0"],
        vec![
            "outputs",
            "download",
            "../escape",
            destination.to_str().unwrap(),
        ],
        vec![
            "outputs",
            "download",
            "friendly.mp4",
            destination.to_str().unwrap(),
        ],
    ] {
        let out = run(&home, &args);
        assert!(!out.status.success());
        let error: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(error["error"]["code"], "operation_failed");
        assert!(!String::from_utf8_lossy(&out.stderr).contains("AWS_ACCESS_KEY_ID"));
    }
    assert_eq!(std::fs::read(&destination).unwrap(), b"keep");
    assert_eq!(std::fs::read(&source).unwrap(), b"{\"result\":true}");
}
