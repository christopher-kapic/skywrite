use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};
fn cli(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(args)
        .env("SKYWRITE_HOME", home)
        .env_remove("OPENROUTER_API_KEY")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .output()
        .unwrap()
}
fn value(out: Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
#[test]
fn offline_project_schema_init_validate_status_and_bounds() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let manifest = temp.path().join("story.json");
    let path = manifest.to_str().unwrap();
    let schema = value(cli(&home, &["project", "schema"]));
    assert_eq!(schema["properties"]["version"]["const"], 1);
    assert_eq!(schema["properties"]["nodes"]["maxItems"], 256);
    assert_eq!(schema["$defs"]["node"]["additionalProperties"], false);
    value(cli(&home, &["project", "init", path]));
    let original = std::fs::read(&manifest).unwrap();
    assert!(!cli(&home, &["project", "init", path]).status.success());
    assert_eq!(std::fs::read(&manifest).unwrap(), original);
    let valid = value(cli(&home, &["project", "validate", path]));
    assert_eq!(valid["valid"], true);
    assert_eq!(valid["paid_request"], false);
    assert_eq!(valid["nodes"], 4);
    let status = value(cli(&home, &["batch", "status", path]));
    assert_eq!(status["counts"]["pending"], 4);
    assert_eq!(status["complete"], false);
    assert!(!home.join("jobs").exists());
    for concurrency in ["0", "9"] {
        let out = cli(
            &home,
            &["project", "run", path, "--concurrency", concurrency],
        );
        assert!(!out.status.success());
        assert_eq!(std::fs::read(&manifest).unwrap(), original);
    }
    assert!(
        !cli(&home, &["project", "resume", path, "--offline"])
            .status
            .success()
    );
    assert!(!home.join("jobs").exists());
    let built = value(cli(&home, &["skills", "read", "projects"]));
    assert_eq!(built["origin"], "builtin");
}

#[test]
fn project_cli_rejects_invalid_graphs_and_preserves_existing_output() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let manifest = temp.path().join("story.json");
    let output = temp.path().join("out");
    std::fs::create_dir(&output).unwrap();
    std::fs::write(output.join("one.png"), b"keep-existing").unwrap();
    let node = serde_json::json!({"id":"one","output":"one.png","model":"fixture/image","action":{"type":"image","prompt":"hello"}});
    let valid = serde_json::json!({"version":1,"output_root":"out","nodes":[node.clone()]});
    std::fs::write(&manifest, serde_json::to_vec(&valid).unwrap()).unwrap();
    let path = manifest.to_str().unwrap();
    let out = cli(&home, &["project", "run", path, "--offline"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unrecorded output exists"));
    assert_eq!(
        std::fs::read(output.join("one.png")).unwrap(),
        b"keep-existing"
    );
    assert!(!home.join("jobs").exists());
    for kind in [
        "empty",
        "too-many",
        "cycle",
        "escape",
        "unknown-field",
        "shell",
    ] {
        let mut invalid = valid.clone();
        match kind {
            "empty" => invalid["nodes"] = serde_json::json!([]),
            "too-many" => {
                invalid["nodes"] = serde_json::json!(
                    (0..257)
                        .map(|i| {
                            let mut n = node.clone();
                            n["id"] = serde_json::json!(format!("n{i}"));
                            n["output"] = serde_json::json!(format!("n{i}.png"));
                            n
                        })
                        .collect::<Vec<_>>()
                )
            }
            "cycle" => invalid["nodes"][0]["depends_on"] = serde_json::json!(["one"]),
            "escape" => invalid["nodes"][0]["output"] = serde_json::json!("../escape.png"),
            "unknown-field" => invalid["nodes"][0]["execute"] = serde_json::json!("false"),
            _ => {
                invalid["nodes"][0]["action"] =
                    serde_json::json!({"type":"shell","command":"false"})
            }
        }
        let bytes = serde_json::to_vec(&invalid).unwrap();
        std::fs::write(&manifest, &bytes).unwrap();
        assert!(
            !cli(&home, &["project", "validate", path]).status.success(),
            "{kind}"
        );
        assert_eq!(std::fs::read(&manifest).unwrap(), bytes);
        assert!(!home.join("jobs").exists());
    }
}
