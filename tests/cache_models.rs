use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
fn cli(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(args)
        .env("SKYWRITE_HOME", root.join("personal"))
        .env("SKYWRITE_CACHE_DIR", root.join("cache"))
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .unwrap()
}
fn cached_catalog(root: &Path, endpoint: &str, data: Value) {
    let key = skywrite::openrouter::cache_key(&[b"catalog-v1", endpoint.as_bytes()]);
    let path = root.join("cache/models").join(format!("{key}.json"));
    skywrite::io::write_json(
        &path,
        &json!({"version":1,"endpoint":endpoint,"fetched_at":1,"body":{"data":data}}),
        false,
    )
    .unwrap();
}
#[test]
fn offline_discovery_preserves_dedicated_caps_and_pricing_without_key() {
    let temp = tempfile::tempdir().unwrap();
    cached_catalog(
        temp.path(),
        "videos/models",
        json!([
            {"id":"test/video","supported_durations":[5],"supported_frame_images":["first_frame","last_frame"],"pricing_skus":{"cents_per_video_output_second":"5"}},
            {"id":"test/upscale","upscale_factor":{"min":1.5,"max":3},"pricing_skus":{"cents_per_megapixel_second_precise":"3.5"}}
        ]),
    );
    cached_catalog(
        temp.path(),
        "models?output_modalities=video",
        json!([
            {"id":"test/video","architecture":{"input_modalities":["text","image"],"output_modalities":["video"]},"pricing":{"prompt":"0","completion":"0"}},
            {"id":"test/upscale","architecture":{"input_modalities":["video"],"output_modalities":["video"]},"pricing":{"prompt":"0","completion":"0"}}
        ]),
    );
    let output = cli(
        temp.path(),
        &["models", "list", "--task", "video-upscaling", "--offline"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["models"].as_array().unwrap().len(), 1);
    assert_eq!(value["models"][0]["id"], "test/upscale");
    assert_eq!(
        value["models"][0]["pricing_skus"]["cents_per_megapixel_second_precise"],
        "3.5"
    );
    assert!(
        value["models"][0]["pricing_note"]
            .as_str()
            .unwrap()
            .contains("do not mean free")
    );
    assert_eq!(value["sources"][0]["stale"], true);
    let output = cli(
        temp.path(),
        &["models", "list", "--task", "analysis", "--offline"],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no usable offline"));
}
#[test]
fn root_help_requires_all_nits_and_shows_composable_tools() {
    let temp = tempfile::tempdir().unwrap();
    let output = cli(temp.path(), &["--help"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("ALL frustrations"));
    assert!(text.contains("skywrite nit"));
    assert!(text.contains("reference frames"));
}
#[test]
fn environment_process_helper() {
    if let Ok(mode) = std::env::var("SKYWRITE_TEST_ENV_HELPER") {
        let explicit = std::env::var("SKYWRITE_TEST_EXPLICIT").ok();
        let result = match mode.as_str() {
            "cache" => skywrite::cache::cache_root(explicit.as_deref().map(Path::new))
                .unwrap()
                .to_string_lossy()
                .to_string(),
            "model" => skywrite::models::resolve_model("analysis", explicit.as_deref()).unwrap(),
            _ => panic!("invalid helper mode"),
        };
        println!("RESULT={result}");
    }
}
#[test]
fn cache_location_and_model_precedence_are_process_local() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("config.json"),
        b"{\"model_defaults\":{\"analysis\":\"saved/model\"}}",
    )
    .unwrap();
    let run = |mode: &str, envs: &[(&str, &str)]| {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "environment_process_helper", "--nocapture"])
            .env("SKYWRITE_TEST_ENV_HELPER", mode)
            .env("HOME", "/sample/home")
            .env("SKYWRITE_HOME", temp.path())
            .env_remove("SKYWRITE_CACHE_DIR")
            .env_remove("XDG_CACHE_HOME")
            .env_remove("SKYWRITE_TEST_EXPLICIT")
            .env_remove("SKYWRITE_ANALYSIS_MODEL")
            .env_remove("OPENROUTER_VISION_MODEL");
        for (key, value) in envs {
            command.env(key, value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("RESULT=").map(str::to_owned))
            .unwrap()
    };
    assert_eq!(run("cache", &[]), "/sample/home/.cache/skywrite");
    assert_eq!(
        run("cache", &[("XDG_CACHE_HOME", "relative")]),
        "/sample/home/.cache/skywrite"
    );
    assert_eq!(run("cache", &[("XDG_CACHE_HOME", "/xdg")]), "/xdg/skywrite");
    assert_eq!(
        run(
            "cache",
            &[("XDG_CACHE_HOME", "/xdg"), ("SKYWRITE_CACHE_DIR", "/app")]
        ),
        "/app"
    );
    assert_eq!(
        run(
            "cache",
            &[
                ("SKYWRITE_CACHE_DIR", "/app"),
                ("SKYWRITE_TEST_EXPLICIT", "/explicit")
            ]
        ),
        "/explicit"
    );
    assert_eq!(run("model", &[]), "saved/model");
    assert_eq!(
        run("model", &[("OPENROUTER_VISION_MODEL", "legacy/model")]),
        "legacy/model"
    );
    assert_eq!(
        run(
            "model",
            &[
                ("OPENROUTER_VISION_MODEL", "legacy/model"),
                ("SKYWRITE_ANALYSIS_MODEL", "task/model")
            ]
        ),
        "task/model"
    );
    assert_eq!(
        run(
            "model",
            &[
                ("SKYWRITE_ANALYSIS_MODEL", "task/model"),
                ("SKYWRITE_TEST_EXPLICIT", "explicit/model")
            ]
        ),
        "explicit/model"
    );
}
