use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_skywrite"));
    command
        .env("SKYWRITE_HOME", root.join("personal"))
        .env("SKYWRITE_CACHE_DIR", root.join("cache"))
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENROUTER_VISION_MODEL")
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("https_proxy", "http://127.0.0.1:9")
        .env("http_proxy", "http://127.0.0.1:9")
        .env("NO_PROXY", "")
        .env("no_proxy", "");
    for task in skywrite::models::TASKS {
        command.env_remove(skywrite::models::task_environment(task).unwrap());
    }
    command
}

fn run(root: &Path, args: &[&str]) -> Value {
    let output = command(root).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn seed_catalog(root: &Path, endpoint: &str, models: Value) {
    let key = skywrite::openrouter::cache_key(&[b"catalog-v1", endpoint.as_bytes()]);
    skywrite::io::write_json(
        &root.join("cache/models").join(format!("{key}.json")),
        &json!({"version":1,"endpoint":endpoint,"fetched_at":1,"body":{"data":models}}),
        false,
    )
    .unwrap();
}

#[test]
fn offline_init_is_idempotent_and_preserves_preferences_and_unknown_fields() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let preview = run(root, &["init", "--dry-run"]);
    assert!(preview["would_change"].as_bool().unwrap());
    assert_eq!(preview["network_requests"], false);
    assert!(
        !root.join("personal").exists(),
        "dry run created personal files"
    );
    let initialized = run(root, &["init"]);
    assert_eq!(initialized["created"], true);
    assert_eq!(initialized["model_defaults"].as_object().unwrap().len(), 8);
    let config_path = root.join("personal/config.json");
    let first = fs::read(&config_path).unwrap();
    assert_eq!(run(root, &["init"])["changed"], false);
    assert_eq!(first, fs::read(&config_path).unwrap());
    let mut custom: Value = serde_json::from_slice(&first).unwrap();
    custom["model_defaults"]["analysis"] = json!("custom/analysis");
    custom["unrelated"] = json!({"keep":[1,"two"]});
    fs::write(&config_path, serde_json::to_vec_pretty(&custom).unwrap()).unwrap();
    let before = fs::read(&config_path).unwrap();
    assert_eq!(run(root, &["init"])["changed"], false);
    assert_eq!(before, fs::read(&config_path).unwrap());
    assert_eq!(
        run(root, &["defaults", "analysis"])["model"],
        "custom/analysis"
    );
    let local = run(root, &["defaults", "image-upscaling"]);
    assert_eq!(local["model"], Value::Null);
    assert_eq!(local["source"], "local_tool");
    assert!(
        !String::from_utf8(fs::read(&config_path).unwrap())
            .unwrap()
            .contains("API_KEY")
    );
}

#[test]
fn task_aliases_default_setters_and_real_dry_run_share_selection() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    assert_eq!(run(root, &["defaults", "image"])["model"], Value::Null);
    let fallback = run(root, &["defaults", "transcription"]);
    assert_eq!(fallback["model"], "openai/whisper-large-v3");
    assert_eq!(fallback["source"], "builtin");
    assert_eq!(
        run(root, &["defaults"])["defaults"]
            .as_array()
            .unwrap()
            .len(),
        9
    );
    let saved = run(root, &["default", "image", "set", "test/saved"]);
    assert_eq!(saved["task"], "image-generation");
    assert_eq!(saved["source"], "saved");
    let candidates = json!([
        {"id":"test/saved","supported_parameters":{}},
        {"id":"test/environment","supported_parameters":{}},
        {"id":"test/explicit","supported_parameters":{}}
    ]);
    seed_catalog(root, "images/models", candidates.clone());
    seed_catalog(root, "models?output_modalities=image", candidates);
    let list = run(root, &["models", "list", "--task", "image", "--offline"]);
    assert_eq!(list["task"], "image-generation");
    assert_eq!(list["models"].as_array().unwrap().len(), 3);
    let mut getter = command(root);
    let output = getter
        .env("SKYWRITE_IMAGE_GENERATION_MODEL", "test/environment")
        .args(["defaults", "image", "get"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let query: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(query["model"], "test/environment");
    assert_eq!(query["saved_model"], "test/saved");
    assert_eq!(query["source"], "environment");
    let output = command(root)
        .env("SKYWRITE_IMAGE_GENERATION_MODEL", "test/environment")
        .args([
            "generate",
            "image",
            "--prompt",
            "dry run only",
            "--offline",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let generated: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(generated["model"], query["model"]);
    let output = command(root)
        .env("SKYWRITE_IMAGE_GENERATION_MODEL", "test/environment")
        .args([
            "generate",
            "image",
            "--prompt",
            "dry run only",
            "--offline",
            "--dry-run",
            "--model",
            "test/explicit",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["model"],
        "test/explicit"
    );
    assert_eq!(
        run(root, &["defaults", "image", "unset"])["model"],
        Value::Null
    );
    run(
        root,
        &["models", "set-default", "image-generation", "test/legacy"],
    );
    assert_eq!(run(root, &["defaults", "image"])["model"], "test/legacy");
}

#[test]
fn selection_child() {
    if std::env::var_os("SKYWRITE_CONFIG_HELPER").is_none() {
        return;
    }
    let explicit = std::env::var("SKYWRITE_CONFIG_EXPLICIT").ok();
    let selection = skywrite::models::selection("analysis", explicit.as_deref()).unwrap();
    let actual = skywrite::models::resolve_model("analysis", explicit.as_deref()).unwrap();
    println!(
        "SELECTION={}",
        json!({"selection":selection,"actual":actual})
    );
}

#[test]
fn legacy_environment_and_explicit_overrides_keep_runtime_contract() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    run(root, &["defaults", "analysis", "set", "saved/analysis"]);
    let child = |envs: &[(&str, &str)]| {
        let mut command = command(root);
        command = {
            let mut child = Command::new(std::env::current_exe().unwrap());
            for (key, value) in command.get_envs() {
                match value {
                    Some(value) => {
                        child.env(key, value);
                    }
                    None => {
                        child.env_remove(key);
                    }
                }
            }
            child
        };
        command
            .args(["--exact", "selection_child", "--nocapture"])
            .env("SKYWRITE_CONFIG_HELPER", "1")
            .env_remove("SKYWRITE_CONFIG_EXPLICIT");
        for (key, value) in envs {
            command.env(key, value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let line = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("SELECTION=").map(str::to_owned))
            .unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };
    for (envs, expected, source) in [
        (vec![], "saved/analysis", "saved"),
        (
            vec![("OPENROUTER_VISION_MODEL", "legacy/analysis")],
            "legacy/analysis",
            "legacy_environment",
        ),
        (
            vec![
                ("OPENROUTER_VISION_MODEL", "legacy/analysis"),
                ("SKYWRITE_ANALYSIS_MODEL", "task/analysis"),
            ],
            "task/analysis",
            "environment",
        ),
        (
            vec![
                ("SKYWRITE_ANALYSIS_MODEL", "task/analysis"),
                ("SKYWRITE_CONFIG_EXPLICIT", "explicit/analysis"),
            ],
            "explicit/analysis",
            "explicit",
        ),
    ] {
        let value = child(&envs);
        assert_eq!(value["actual"], expected);
        assert_eq!(value["selection"]["model"], expected);
        assert_eq!(value["selection"]["source"], source);
    }
    fs::write(root.join("personal/config.json"), b"broken JSON").unwrap();
    assert_eq!(
        child(&[("SKYWRITE_CONFIG_EXPLICIT", "explicit/analysis")])["actual"],
        "explicit/analysis"
    );
    assert_eq!(
        child(&[("SKYWRITE_ANALYSIS_MODEL", "task/analysis")])["actual"],
        "task/analysis"
    );
}

#[test]
fn concurrent_init_and_setters_use_one_lock_without_losing_choices() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("personal")).unwrap();
    fs::write(
        root.join("personal/config.json"),
        br#"{"model_defaults":{"analysis":"keep/model"},"unrelated":{"keep":true}}"#,
    )
    .unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("personal/config.lock"))
        .unwrap();
    lock.lock().unwrap();
    let mut children: Vec<_> = (0..4)
        .map(|_| {
            command(root)
                .args(["init"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for (task, model) in [
        ("image", "custom/image"),
        ("video", "custom/video"),
        ("music", "custom/music"),
    ] {
        children.push(
            command(root)
                .args(["defaults", task, "set", model])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    for child in &mut children {
        assert!(
            child.try_wait().unwrap().is_none(),
            "mutation ignored held config lock"
        );
    }
    lock.unlock().unwrap();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let saved: Value =
        serde_json::from_slice(&fs::read(root.join("personal/config.json")).unwrap()).unwrap();
    assert_eq!(saved["unrelated"]["keep"], true);
    assert_eq!(saved["model_defaults"]["analysis"], "keep/model");
    for (task, model) in [
        ("image-generation", "custom/image"),
        ("video-generation", "custom/video"),
        ("music-generation", "custom/music"),
    ] {
        assert_eq!(saved["model_defaults"][task], model);
    }
    assert_eq!(saved["model_defaults"].as_object().unwrap().len(), 8);
}

#[test]
fn malformed_and_oversized_configs_are_preserved_on_init_failure() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("personal")).unwrap();
    let path = root.join("personal/config.json");
    for bytes in [
        b"{broken".to_vec(),
        serde_json::to_vec(&json!({"unrelated":"x".repeat(65_400)})).unwrap(),
    ] {
        fs::write(&path, &bytes).unwrap();
        let output = command(root).args(["init"]).output().unwrap();
        assert!(
            !output.status.success(),
            "invalid/oversized merged config accepted"
        );
        assert!(
            serde_json::from_slice::<Value>(&output.stderr)
                .unwrap()
                .get("error")
                .is_some()
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn an_invalid_override_is_visible_without_hiding_a_successful_saved_change() {
    let temp = tempfile::tempdir().unwrap();
    let output = command(temp.path())
        .env("SKYWRITE_IMAGE_GENERATION_MODEL", "invalid model")
        .args(["defaults", "image", "set", "test/saved"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["saved_model"], "test/saved");
    assert_eq!(value["model"], Value::Null);
    assert_eq!(value["source"], "invalid_override");
    assert!(
        value["selection_error"]
            .as_str()
            .unwrap()
            .contains("model ID")
    );
    assert_eq!(
        run(temp.path(), &["defaults", "image"])["model"],
        "test/saved"
    );
    let output = command(temp.path())
        .env("SKYWRITE_IMAGE_GENERATION_MODEL", "invalid model")
        .args([
            "generate",
            "image",
            "--prompt",
            "no request",
            "--dry-run",
            "--offline",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("model ID"));
}
