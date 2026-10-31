use serde_json::{Value, json};
use skywrite::workspace::{self, ContentCommand, ContentKind};
use std::{fs, path::Path, process::Command};

fn child(home: &Path, case: &str) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "store_child", "--nocapture"])
        .env("SKYWRITE_HOME", home)
        .env("SKYWRITE_STORE_TEST", case)
        .output()
        .unwrap()
}
fn success(output: std::process::Output) {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn personal_markdown_round_trip_and_builtin_shadow() {
    let t = tempfile::tempdir().unwrap();
    success(child(t.path(), "content"));
}
#[test]
fn bounded_regular_files_and_errors_preserve_data() {
    let t = tempfile::tempdir().unwrap();
    success(child(t.path(), "safety"));
}
#[test]
fn concurrent_nits_are_unique_and_capture_only_description() {
    let t = tempfile::tempdir().unwrap();
    success(child(t.path(), "nits"));
}
#[test]
fn concurrent_config_processes_preserve_all_tasks_and_unknown_fields() {
    let t = tempfile::tempdir().unwrap();
    success(child(t.path(), "config"));
}
#[test]
fn cli_content_and_defaults_are_composable_json() {
    let temp = tempfile::tempdir().unwrap();
    let invoke = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_skywrite"))
            .args(args)
            .env("SKYWRITE_HOME", temp.path())
            .output()
            .unwrap()
    };
    for args in [
        vec!["styles", "create", "tone", "--content", "Warm light"],
        vec!["styles", "update", "tone", "--content", "Soft light"],
        vec!["styles", "read", "tone"],
        vec!["styles", "list"],
        vec!["skills", "read", "takumi"],
        vec!["models", "set-default", "analysis", "provider/model"],
        vec!["models", "defaults"],
        vec!["models", "unset-default", "analysis"],
        vec!["nit", "Something was confusing"],
    ] {
        let output = invoke(&args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).expect("single JSON output");
    }
    let duplicate = invoke(&["styles", "create", "tone", "--content", "No"]);
    assert!(!duplicate.status.success());
    assert!(
        serde_json::from_slice::<Value>(&duplicate.stderr)
            .unwrap()
            .get("error")
            .is_some()
    );
    let both = invoke(&[
        "styles",
        "create",
        "other",
        "--content",
        "No",
        "--file",
        "missing.md",
    ]);
    assert!(!both.status.success());
}
fn create(kind: ContentKind, name: &str, content: &str) -> anyhow::Result<Value> {
    workspace::execute_content(
        kind,
        ContentCommand::Create {
            name: name.into(),
            file: None,
            content: Some(content.into()),
        },
    )
}
fn read(kind: ContentKind, name: &str) -> anyhow::Result<Value> {
    workspace::execute_content(kind, ContentCommand::Read { name: name.into() })
}
#[test]
fn store_child() {
    let Ok(case) = std::env::var("SKYWRITE_STORE_TEST") else {
        return;
    };
    let home = workspace::personal_home().unwrap();
    match case.as_str() {
        "content" => {
            assert_eq!(
                workspace::execute_content(ContentKind::Styles, ContentCommand::List).unwrap()["items"],
                json!([])
            );
            assert!(!home.join("styles").exists());
            let value = create(ContentKind::Styles, "z_style", "# Original\nWarm light.").unwrap();
            assert_eq!(value["origin"], "user");
            let creators: Vec<_> = (0..16)
                .map(|i| {
                    std::thread::spawn(move || {
                        create(ContentKind::Styles, "race", &format!("winner-{i}"))
                    })
                })
                .collect();
            let results: Vec<_> = creators
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect();
            assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
            assert!(
                read(ContentKind::Styles, "race").unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .starts_with("winner-")
            );

            assert_eq!(value["content"], "# Original\nWarm light.");
            assert_eq!(value["sha256"].as_str().unwrap().len(), 64);
            assert!(create(ContentKind::Styles, "z_style", "oops").is_err());
            let source = home.join("source.md");
            fs::write(&source, "# Revised\nSoft light.").unwrap();
            workspace::execute_content(
                ContentKind::Styles,
                ContentCommand::Update {
                    name: "z_style".into(),
                    file: Some(source.clone()),
                    content: None,
                },
            )
            .unwrap();
            assert_eq!(
                fs::read_to_string(source).unwrap(),
                "# Revised\nSoft light."
            );
            assert_eq!(
                read(ContentKind::Styles, "z_style").unwrap()["content"],
                "# Revised\nSoft light."
            );
            create(ContentKind::Styles, "alpha", "First").unwrap();
            let list =
                workspace::execute_content(ContentKind::Styles, ContentCommand::List).unwrap();
            assert_eq!(list["items"][0]["name"], "alpha");
            assert!(list["items"][0].get("content").is_none());
            assert_eq!(list["items"][2]["name"], "z_style");
            let builtins =
                workspace::execute_content(ContentKind::Skills, ContentCommand::List).unwrap();
            let names: Vec<_> = builtins["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["name"].as_str().unwrap())
                .collect();
            assert_eq!(
                names,
                vec![
                    "character-reference",
                    "composing-tools",
                    "fcpxml",
                    "ffmpeg",
                    "hyperframes",
                    "projects",
                    "remotion",
                    "synchronization",
                    "takumi",
                    "technical-explainer"
                ]
            );
            assert_eq!(
                read(ContentKind::Skills, "ffmpeg").unwrap()["origin"],
                "builtin"
            );
            assert!(
                workspace::execute_content(
                    ContentKind::Skills,
                    ContentCommand::Update {
                        name: "ffmpeg".into(),
                        file: None,
                        content: Some("no".into())
                    }
                )
                .is_err()
            );
            create(ContentKind::Skills, "ffmpeg", "My explicit override").unwrap();
            assert_eq!(
                read(ContentKind::Skills, "ffmpeg").unwrap()["origin"],
                "user"
            );
        }
        "safety" => {
            for name in ["", "../escape", "a/b", ".", "a\n", "é"] {
                assert!(
                    create(ContentKind::Styles, name, "safe").is_err(),
                    "{name:?}"
                );
            }
            assert!(create(ContentKind::Styles, &"a".repeat(65), "safe").is_err());
            assert!(create(ContentKind::Styles, "large", &"a".repeat(256 * 1024 + 1)).is_err());
            assert!(create(ContentKind::Styles, "nul", "a\0b").is_err());
            assert!(
                workspace::execute_content(
                    ContentKind::Styles,
                    ContentCommand::Update {
                        name: "missing".into(),
                        file: None,
                        content: Some("no".into())
                    }
                )
                .is_err()
            );
            let source = home.join("bad-utf8.md");
            fs::write(&source, [255]).unwrap();
            assert!(
                workspace::execute_content(
                    ContentKind::Styles,
                    ContentCommand::Create {
                        name: "invalid".into(),
                        file: Some(source),
                        content: None
                    }
                )
                .is_err()
            );
            assert!(
                workspace::execute_content(
                    ContentKind::Styles,
                    ContentCommand::Create {
                        name: "directory".into(),
                        file: Some(home.clone()),
                        content: None
                    }
                )
                .is_err()
            );
            create(ContentKind::Styles, "original", "do not lose").unwrap();
            let original = home.join("styles/original.md");
            fs::write(home.join("styles/oversize.md"), vec![b'x'; 256 * 1024 + 1]).unwrap();
            assert!(read(ContentKind::Styles, "oversize").is_err());
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&original, home.join("styles/link.md")).unwrap();
                assert!(read(ContentKind::Styles, "link").is_err());
                assert!(
                    workspace::execute_content(
                        ContentKind::Styles,
                        ContentCommand::Update {
                            name: "link".into(),
                            file: None,
                            content: Some("no".into())
                        }
                    )
                    .is_err()
                );
            }
            assert_eq!(fs::read_to_string(original).unwrap(), "do not lose");
            assert!(workspace::record_nit("  ").is_err());
            assert!(workspace::record_nit(&"x".repeat(8193)).is_err());
            assert!(workspace::set_default_model("other", "a/model").is_err());
            assert!(workspace::set_default_model("analysis", "a model").is_err());
            let malformed = b"{ invalid config";
            fs::write(home.join("config.json"), malformed).unwrap();
            assert!(workspace::set_default_model("analysis", "a/model").is_err());
            assert_eq!(fs::read(home.join("config.json")).unwrap(), malformed);
            fs::write(home.join("config.json"), vec![b' '; 64 * 1024 + 1]).unwrap();
            assert!(workspace::model_defaults().is_err());
            assert_eq!(
                fs::metadata(home.join("config.json")).unwrap().len(),
                64 * 1024 + 1
            );
        }
        "nits" => {
            let threads: Vec<_> = (0..32)
                .map(|i| {
                    std::thread::spawn(move || {
                        workspace::record_nit(&format!("Feedback {i}")).unwrap()
                    })
                })
                .collect();
            let mut paths = std::collections::BTreeSet::new();
            for thread in threads {
                let value = thread.join().unwrap();
                assert!(paths.insert(value["path"].as_str().unwrap().to_owned()));
            }
            assert_eq!(fs::read_dir(home.join("nits")).unwrap().count(), 32);
            for path in paths {
                let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
                let keys: Vec<_> = value
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect();
                assert_eq!(
                    keys,
                    [
                        "app_version",
                        "created_unix_nanoseconds",
                        "created_unix_seconds",
                        "description",
                        "version"
                    ]
                );
                assert_eq!(value["app_version"], env!("CARGO_PKG_VERSION"));
            }
        }
        "config" => {
            assert!(workspace::model_defaults().unwrap().is_empty());
            fs::write(
                home.join("config.json"),
                b"{\"version\":1,\"other\":{\"preserved\":true}}",
            )
            .unwrap();
            let children: Vec<_> = workspace::MODEL_TASKS
                .iter()
                .map(|task| {
                    Command::new(std::env::current_exe().unwrap())
                        .args(["--exact", "store_child", "--nocapture"])
                        .env("SKYWRITE_HOME", &home)
                        .env("SKYWRITE_STORE_TEST", "setter")
                        .env("SKYWRITE_STORE_TASK", task)
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .spawn()
                        .unwrap()
                })
                .collect();
            for child in children {
                success(child.wait_with_output().unwrap());
            }
            let defaults = workspace::model_defaults().unwrap();
            assert_eq!(defaults.len(), workspace::MODEL_TASKS.len());
            for task in workspace::MODEL_TASKS {
                assert_eq!(defaults[*task], format!("provider/{task}"));
            }
            assert_eq!(
                workspace::get_default_model("analysis").unwrap().as_deref(),
                Some("provider/analysis")
            );
            workspace::unset_default_model("analysis").unwrap();
            assert!(workspace::get_default_model("analysis").unwrap().is_none());
            assert_eq!(
                workspace::model_defaults().unwrap().len(),
                workspace::MODEL_TASKS.len() - 1
            );
            let config: Value =
                serde_json::from_slice(&fs::read(home.join("config.json")).unwrap()).unwrap();
            assert_eq!(config["other"]["preserved"], true);
            assert!(home.join("config.lock").is_file());
        }
        "setter" => {
            let task = std::env::var("SKYWRITE_STORE_TASK").unwrap();
            workspace::set_default_model(&task, &format!("provider/{task}")).unwrap();
        }
        _ => panic!("unknown helper case"),
    }
}
