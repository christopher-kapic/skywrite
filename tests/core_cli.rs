use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Output},
};
fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(args)
        .output()
        .unwrap()
}
fn json_output(out: Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("single JSON response")
}
fn fixture(path: &Path) {
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-n",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
#[test]
fn default_errors_are_json_and_help_is_available() {
    let empty = cli(&[]);
    assert!(!empty.status.success());
    let value: Value = serde_json::from_slice(&empty.stderr).unwrap();
    assert_eq!(value["error"]["code"], "invalid_arguments");
    let help = cli(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("skywrite"));
    let missing = cli(&["inspect", "/deliberately/missing/media.mp4"]);
    assert!(!missing.status.success());
    let error: Value = serde_json::from_slice(&missing.stderr).unwrap();
    assert_eq!(error["error"]["code"], "operation_failed");
}
#[test]
fn inspects_hostile_filename_and_samples_real_frames_without_network() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("--unsafe 'quoted'; dollar$.mp4");
    fixture(&input);
    let input_str = input.to_str().unwrap();
    let metadata = json_output(cli(&["inspect", input_str]));
    assert!((metadata["media"][0]["duration"].as_f64().unwrap() - 2.0).abs() < 0.1);
    assert_eq!(metadata["media"][0]["streams"][0]["width"], 160);
    let frames = temp.path().join("frames");
    let report = temp.path().join("analysis.json");
    let analysis = json_output(cli(&[
        "analyze",
        input_str,
        "--local",
        "--frames",
        "2",
        "--frames-dir",
        frames.to_str().unwrap(),
        "--output",
        report.to_str().unwrap(),
    ]));
    assert_eq!(analysis["frames"].as_array().unwrap().len(), 2);
    assert_eq!(analysis["frames"][0]["time"], 0.5);
    assert_eq!(analysis["frames"][1]["time"], 1.5);
    for frame in analysis["frames"].as_array().unwrap() {
        assert!(Path::new(frame["path"].as_str().unwrap()).is_file());
    }
    assert!(analysis["understanding"].is_null());
    assert!(
        analysis["coverage"]["visual"]
            .as_str()
            .unwrap()
            .contains("sparse")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(report).unwrap()).unwrap(),
        analysis
    );
    let original = std::fs::read(&input).unwrap();
    assert!(
        !cli(&[
            "analyze",
            input_str,
            "--local",
            "--output",
            input_str,
            "--overwrite"
        ])
        .status
        .success()
    );
    assert_eq!(std::fs::read(&input).unwrap(), original);
}
#[test]
fn transcript_sync_cli_produces_directionally_clear_mapping() {
    let temp = tempfile::tempdir().unwrap();
    let reference = temp.path().join("reference.json");
    let asset = temp.path().join("asset.json");
    let sentence = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty thirty forty fifty sixty seventy eighty ninety hundred";
    let words = sentence
        .split_whitespace()
        .enumerate()
        .map(|(i, w)| json!({"text":w,"start":i as f64*2.0,"end":i as f64*2.0+0.4}))
        .collect::<Vec<_>>();
    let asset_json =
        json!({"version":1,"source":"asset.wav","duration":100,"text":sentence,"words":words});
    let mut reference_json = asset_json.clone();
    reference_json["source"] = json!("reference.wav");
    reference_json["duration"] = json!(120);
    for word in reference_json["words"].as_array_mut().unwrap() {
        word["start"] = json!(12.0 + 1.01 * word["start"].as_f64().unwrap());
        word["end"] = json!(12.0 + 1.01 * word["end"].as_f64().unwrap());
    }
    std::fs::write(&reference, serde_json::to_vec(&reference_json).unwrap()).unwrap();
    std::fs::write(&asset, serde_json::to_vec(&asset_json).unwrap()).unwrap();
    let mapping = json_output(cli(&[
        "sync",
        "--reference",
        reference.to_str().unwrap(),
        "--asset",
        asset.to_str().unwrap(),
        "--method",
        "transcript",
    ]));
    assert!(
        (mapping["offset_seconds"].as_f64().unwrap() - 12.0).abs() < 0.02,
        "{mapping}"
    );
    assert!(
        (mapping["scale"].as_f64().unwrap() - 1.01).abs() < 1e-5,
        "{mapping}"
    );
    assert!((mapping["playback_rate"].as_f64().unwrap() - 1.0 / 1.01).abs() < 1e-5);
}
#[test]
fn audio_extraction_preserves_stream_delay_and_range_time_origin() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("delayed.mkv");
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-n",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:size=160x90:rate=30:duration=3",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=16000:duration=2",
            "-filter_complex",
            "[1:a]asetpts=PTS+0.4/TB[a]",
            "-map",
            "0:v",
            "-map",
            "[a]",
            "-c:v",
            "libx264",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info = skywrite::media::probe(&source).unwrap();
    for (start, expected) in [(0.0, 0.4), (0.2, 0.2), (0.8, 0.0)] {
        let wav = temp.path().join(format!("sample-{start}.wav"));
        skywrite::media::extract_audio(&info, &wav, start, 1.0).unwrap();
        let bytes = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&wav)
            .args(["-f", "s16le", "-ac", "1", "-ar", "16000", "pipe:1"])
            .output()
            .unwrap()
            .stdout;
        let first = bytes
            .chunks_exact(2)
            .position(|b| i16::from_le_bytes([b[0], b[1]]).abs() > 100)
            .unwrap() as f64
            / 16000.0;
        assert!(
            (first - expected).abs() < 0.02,
            "start={start},first={first},expected={expected}"
        );
        assert!((bytes.len() as f64 / 32000.0 - 1.0).abs() < 0.01);
    }
}
#[test]
fn cached_word_transcription_uses_selected_source_time_and_rejects_stale_source() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("speech.wav");
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-n",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=317:sample_rate=16000:duration=22",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&input)
        .output()
        .unwrap();
    assert!(out.status.success());
    let info = skywrite::media::probe(&input).unwrap();
    let cache = temp.path().join("cache");
    std::fs::create_dir(&cache).unwrap();
    for (index, offset) in [1.0, 11.0].into_iter().enumerate() {
        let audio = temp.path().join(format!("chunk-{index}.wav"));
        skywrite::media::extract_audio(&info, &audio, offset, 10.0).unwrap();
        let request = json!({"model":"cached-word-fixture","input_audio":{"data":STANDARD.encode(std::fs::read(audio).unwrap()),"format":"wav"},"response_format":"verbose_json","timestamp_granularities":["word"]});
        let bytes = serde_json::to_vec(&request).unwrap();
        let hash = skywrite::openrouter::cache_key(&[
            b"https://openrouter.ai/api/v1",
            b"audio/transcriptions",
            &bytes,
        ]);
        let word = if index == 0 { "hello" } else { "world" };
        let response =
            json!({"text":word,"words":[{"word":word,"start":0.25,"end":0.5}],"usage":{"cost":0}});
        std::fs::write(
            cache.join(format!("{hash}.json")),
            serde_json::to_vec(&response).unwrap(),
        )
        .unwrap();
    }
    let transcript_path = temp.path().join("transcript.json");
    let out = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .env_remove("OPENROUTER_API_KEY")
        .args([
            "transcribe",
            input.to_str().unwrap(),
            "--model",
            "cached-word-fixture",
            "--start",
            "1",
            "--duration",
            "20",
            "--chunk-seconds",
            "10",
            "--cache-dir",
            cache.to_str().unwrap(),
            "--output",
            transcript_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let transcript = json_output(out);
    assert_eq!(transcript["words"][0]["start"], 1.25);
    assert_eq!(transcript["words"][1]["start"], 11.25);
    assert_eq!(transcript["usage"]["cache_hits"], 2);
    assert_eq!(
        transcript["usage"]["source_sha256"].as_str().unwrap().len(),
        64
    );
    let frames = temp.path().join("frames");
    let analyzed = json_output(cli(&[
        "analyze",
        input.to_str().unwrap(),
        "--local",
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--frames-dir",
        frames.to_str().unwrap(),
    ]));
    assert_eq!(
        analyzed["coverage"]["transcript_content_identity_verified"],
        true
    );
    assert_eq!(analyzed["coverage"]["audio_sample_duration"], 0.0);
    let mut changed = std::fs::read(&input).unwrap();
    let index = changed.len() - 200;
    changed[index] ^= 1;
    std::fs::write(&input, changed).unwrap();
    let stale = cli(&[
        "analyze",
        input.to_str().unwrap(),
        "--local",
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--frames-dir",
        frames.to_str().unwrap(),
    ]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("source content has changed"));
}
