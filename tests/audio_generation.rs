use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
fn cli(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(args)
        .env("SKYWRITE_HOME", home)
        .env_remove("OPENROUTER_API_KEY")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .output()
        .unwrap()
}
fn seed_catalog(root: &Path, endpoint: &str, body: Value) {
    let key = skywrite::openrouter::cache_key(&[b"catalog-v1", endpoint.as_bytes()]);
    let path = root.join("models").join(format!("{key}.json"));
    skywrite::io::write_json(&path,&json!({"version":1,"endpoint":endpoint,"fetched_at":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),"body":body}),false).unwrap();
}
#[test]
fn cached_audio_recovery_decodes_publishes_and_replays_without_network() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let wav = temp.path().join("valid.wav");
    let mut args = skywrite::process::args(&[
        "-v",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=0.2",
        "-acodec",
        "pcm_s16le",
    ]);
    args.push(wav.as_os_str().to_owned());
    skywrite::process::run("ffmpeg", &args, Duration::from_secs(10), 1024 * 1024).unwrap();
    let id = "c".repeat(64);
    let dir = home.join("jobs").join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let job = json!({"version":1,"id":id,"task":"music-generation","model":"google/lyria-3-clip-preview","status":"received","created_at":1,"updated_at":1,"request":{},"references":[],"remote_id":null,"generation_id":null,"usage":{"cost":0.04},"artifact":null,"error":null});
    skywrite::io::write_json(&dir.join("job.json"), &job, false).unwrap();
    let response = json!({"audio":{"data":STANDARD.encode(std::fs::read(&wav).unwrap())},"usage":{"cost":0.04},"generation_id":"gen-fixture","text":"<instrumental>"});
    skywrite::io::write_json(&dir.join("response.json"), &response, false).unwrap();
    let out = temp.path().join("published.mp3");
    let r = cli(
        &home,
        &["jobs", "resume", &id, "--output", out.to_str().unwrap()],
    );
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let result: Value = serde_json::from_slice(&r.stdout).unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["output_text"], "<instrumental>");
    assert_eq!(result["usage"]["cost"], 0.04);
    assert!(skywrite::media::probe(&out).unwrap().has_audio());
    let replay = temp.path().join("replay.wav");
    let r = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(["jobs", "resume", &id, "--output", replay.to_str().unwrap()])
        .env("SKYWRITE_HOME", &home)
        .env("PATH", "")
        .env_remove("OPENROUTER_API_KEY")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    assert_eq!(
        std::fs::read(&replay).unwrap(),
        std::fs::read(&wav).unwrap()
    );
    let kept = std::fs::read(&out).unwrap();
    let r = cli(
        &home,
        &["jobs", "resume", &id, "--output", out.to_str().unwrap()],
    );
    assert!(!r.status.success());
    assert_eq!(std::fs::read(&out).unwrap(), kept);
}
#[test]
fn invalid_audio_recovery_preserves_state_response_cost_and_existing_output() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let id = "d".repeat(64);
    let dir = home.join("jobs").join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let job = json!({"version":1,"id":id,"task":"speech-generation","model":"hexgrad/kokoro-82m","status":"received","created_at":1,"updated_at":1,"request":{},"references":[],"remote_id":null,"generation_id":null,"usage":{"cost":0.01},"artifact":null,"error":null});
    skywrite::io::write_json(&dir.join("job.json"), &job, false).unwrap();
    skywrite::io::write_json(&dir.join("response.json"),&json!({"audio":{"data":STANDARD.encode({let mut b=Vec::new();b.extend(b"RIFF");b.extend(37u32.to_le_bytes());b.extend(b"WAVEfmt ");b.extend(16u32.to_le_bytes());b.extend(1u16.to_le_bytes());b.extend(1u16.to_le_bytes());b.extend(44100u32.to_le_bytes());b.extend(88200u32.to_le_bytes());b.extend(2u16.to_le_bytes());b.extend(16u16.to_le_bytes());b.extend(b"data");b.extend(1u32.to_le_bytes());b.push(1);b})},"usage":{"cost":0.01}}),false).unwrap();
    let response = std::fs::read(dir.join("response.json")).unwrap();
    let out = temp.path().join("existing.mp3");
    std::fs::write(&out, b"important previous output").unwrap();
    let r = cli(
        &home,
        &[
            "jobs",
            "resume",
            &id,
            "--output",
            out.to_str().unwrap(),
            "--overwrite",
        ],
    );
    assert!(!r.status.success());
    assert_eq!(std::fs::read(&out).unwrap(), b"important previous output");
    assert_eq!(std::fs::read(dir.join("response.json")).unwrap(), response);
    let state: Value = skywrite::io::read_json(&dir.join("job.json")).unwrap();
    assert_eq!(state["status"], "received");
    assert_eq!(state["usage"]["cost"], 0.01);
    assert!(!dir.join("result.mp3").exists());
}
#[test]
fn offline_plans_validate_voice_output_kind_and_script_preservation() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let cache = temp.path().join("cache");
    seed_catalog(
        &cache,
        "models?output_modalities=speech",
        json!({"data":[{"id":"voice/test","supported_voices":["narrator"],"architecture":{"input_modalities":["text"],"output_modalities":["speech"]}}]}),
    );
    seed_catalog(
        &cache,
        "models?output_modalities=audio",
        json!({"data":[{"id":"google/lyria-test","architecture":{"input_modalities":["text"],"output_modalities":["audio","text"]},"pricing":{"prompt":"0","completion":"0"}}]}),
    );
    let common = [
        "--cache-dir",
        cache.to_str().unwrap(),
        "--offline",
        "--dry-run",
    ];
    let mut args = vec![
        "generate",
        "speech",
        "--text",
        "Hello",
        "--model",
        "voice/test",
        "--voice",
        "narrator",
    ];
    args.extend(common);
    let r = cli(&home, &args);
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let plan: Value = serde_json::from_slice(&r.stdout).unwrap();
    assert_eq!(plan["paid_request"], false);
    assert!(!home.join("jobs").exists());
    let bad = temp.path().join("bad.mp4");
    args.extend(["--output", bad.to_str().unwrap()]);
    let r = cli(&home, &args);
    assert!(!r.status.success());
    let script = temp.path().join("script.mp3");
    std::fs::write(&script, b"Do not overwrite this narration script.").unwrap();
    let mut args = vec![
        "generate",
        "speech",
        "--file",
        script.to_str().unwrap(),
        "--model",
        "voice/test",
        "--voice",
        "narrator",
        "--output",
        script.to_str().unwrap(),
        "--overwrite",
    ];
    args.extend(common);
    let r = cli(&home, &args);
    assert!(!r.status.success());
    assert_eq!(
        std::fs::read(&script).unwrap(),
        b"Do not overwrite this narration script."
    );
    let r = cli(
        &home,
        &[
            "models",
            "list",
            "--task",
            "music-generation",
            "--cache-dir",
            cache.to_str().unwrap(),
            "--offline",
        ],
    );
    assert!(r.status.success());
    let data: Value = serde_json::from_slice(&r.stdout).unwrap();
    assert!(
        data["models"][0]["pricing_note"]
            .as_str()
            .unwrap()
            .contains("NOT mean free")
    );
}
