use serde_json::Value;
use std::{path::Path, process::Command, time::Duration};
fn cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(args)
        .env_remove("OPENROUTER_API_KEY")
        .output()
        .unwrap()
}
#[test]
fn local_resize_pixels_and_input_preservation() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("input.png");
    let output = temp.path().join("enlarged.png");
    let mut args = skywrite::process::args(&[
        "-v",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "color=orange:s=64x48:d=0.1",
        "-frames:v",
        "1",
    ]);
    args.push(input.as_os_str().to_owned());
    skywrite::process::run("ffmpeg", &args, Duration::from_secs(15), 1024 * 1024).unwrap();
    let original = std::fs::read(&input).unwrap();
    let result = cli(&[
        "upscale",
        "image",
        input.to_str().unwrap(),
        "--factor",
        "2",
        "--output",
        output.to_str().unwrap(),
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let json: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(json["ai"], false);
    assert_eq!(json["paid_request"], false);
    assert_eq!(json["width"], 128);
    assert_eq!(json["height"], 96);
    let info = skywrite::media::probe(Path::new(output.to_str().unwrap())).unwrap();
    assert_eq!(info.video().unwrap().width, Some(128));
    assert_eq!(info.video().unwrap().height, Some(96));
    let source_alias = cli(&[
        "upscale",
        "image",
        input.to_str().unwrap(),
        "--output",
        input.to_str().unwrap(),
        "--overwrite",
    ]);
    assert!(!source_alias.status.success());
    assert_eq!(std::fs::read(&input).unwrap(), original);
    let saved = std::fs::read(&output).unwrap();
    let existing = cli(&[
        "upscale",
        "image",
        input.to_str().unwrap(),
        "--output",
        output.to_str().unwrap(),
    ]);
    assert!(!existing.status.success());
    assert_eq!(std::fs::read(output).unwrap(), saved);
}
