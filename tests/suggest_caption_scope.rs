//! Offline edit-consumer proof using supplied provider protocol word times.
use serde_json::{Value, json};
use std::process::Command;

#[test]
fn cached_caption_paths_require_explicit_authority_before_validation() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("speech.wav");
    let output = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=0.1*sin(2*PI*(317*t+2*t*t)):s=16000:d=4",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&source)
        .output()
        .unwrap();
    assert!(output.status.success());
    let source_before = std::fs::read(&source).unwrap();
    let hash = skywrite::media::file_sha256(&source).unwrap();
    let mut report: skywrite::analysis::Analysis = serde_json::from_value(json!({
        "version":1,"media":skywrite::media::probe(&source).unwrap(),
        "range":{"start":1.0,"duration":1.0},"frames":[],"coverage":{"visual":"sparse"},
        "transcript":{"version":1,"source":source,"duration":4.0,"text":"DO NOT USE UNBOUNDED SUMMARY","usage":{"source_sha256":hash},
        "words":[{"text":"outside","start":0.1,"end":0.2},{"text":"supplied-word","start":1.300123,"end":1.600456},{"text":"later","start":2.4,"end":2.7}]},
        "understanding":null,"model":null,"usage":null,"cached":false,"source_sha256":hash
    })).unwrap();
    let transcript = report.transcript.take().unwrap();
    let transcript_path = dir.path().join("words.json");
    skywrite::io::write_json(&transcript_path, &transcript, false).unwrap();
    let report_path = dir.path().join("analysis.json");
    skywrite::io::write_json(&report_path, &report, false).unwrap();
    let schema = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .arg("schema")
        .output()
        .unwrap();
    assert!(schema.status.success());
    let schema: Value = serde_json::from_slice(&schema.stdout).unwrap();
    let mut reports = [report];
    skywrite::analysis::attach_transcripts(&mut reports, &[transcript]).unwrap();
    let context = skywrite::analysis::suggestion_context(&reports).unwrap();
    let encoded = serde_json::to_string(&context).unwrap();
    assert!(encoded.contains("1.300123"));
    assert!(!encoded.contains("DO NOT USE UNBOUNDED SUMMARY"));
    assert!(!encoded.contains("outside"));
    let brief = "Preserve supplied word precision with separate dialogue audio";
    let prompt = skywrite::analysis::suggest_prompt(brief, &schema["edit"], &encoded);
    let request = json!({"model":"fixture/dialogue","messages":[{"role":"user","content":prompt}],"response_format":{"type":"json_object"},"max_tokens":4000,"temperature":0.2});
    let key = skywrite::openrouter::cache_key(&[
        b"https://openrouter.ai/api/v1",
        b"chat/completions",
        &serde_json::to_vec(&request).unwrap(),
    ]);
    let draft = json!({"version":2,"name":"Supplied word","width":64,"height":64,"fps":{"num":30,"den":1},"duration":1.0,"audio":[{"clips":[{"start":0.0,"source":source,"source_in":1.300123,"duration":0.5,"speed":1.0,"volume":1.0}]}]});
    let cache = dir.path().join("cache/responses");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        cache.join(format!("{key}.json")),
        serde_json::to_vec(
            &json!({"choices":[{"message":{"content":serde_json::to_string(&draft).unwrap()}}]}),
        )
        .unwrap(),
    )
    .unwrap();
    let output_path = dir.path().join("edit.json");
    let output = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .env_remove("OPENROUTER_API_KEY")
        .env("SKYWRITE_HOME", dir.path().join("personal"))
        .args(["edit", "suggest", "--analysis"])
        .arg(&report_path)
        .arg("--transcript")
        .arg(&transcript_path)
        .args([
            "--brief",
            brief,
            "--model",
            "fixture/dialogue",
            "--cache-dir",
        ])
        .arg(dir.path().join("cache"))
        .arg("--output")
        .arg(&output_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["audio"][0]["clips"][0]["source_in"], 1.300123);
    assert_eq!(result["audio"][0]["clips"][0]["duration"], 0.5);
    assert!(String::from_utf8_lossy(&output.stderr).contains("\"cached\":true"));
    assert_eq!(std::fs::read(&source).unwrap(), source_before);
    let loaded = skywrite::timeline::load_and_validate(&output_path).unwrap();
    assert_eq!(loaded.summary()["audio_clip_count"], 1);
    let transcript_before = std::fs::read(&transcript_path).unwrap();
    let report_before = std::fs::read(&report_path).unwrap();
    let subtitle = dir.path().join("private.srt");
    let subtitle_bytes = b"1\n00:00:00,000 --> 00:00:00,500\nOwned private subtitle\n";
    std::fs::write(&subtitle, subtitle_bytes).unwrap();
    for (name, caption, accepted) in [
        ("unauthorized", &subtitle, false),
        ("authorized", &transcript_path, true),
    ] {
        let mut next = draft.clone();
        next["duration"] = json!(4.0);
        next["captions"] = json!([{"source":caption}]);
        // This unauthorized file is valid: removing the guard would accept it.
        let edit: skywrite::timeline::Edit = serde_json::from_value(next.clone()).unwrap();
        assert!(skywrite::timeline::validate(&edit, dir.path()).is_ok());
        std::fs::write(
            cache.join(format!("{key}.json")),
            serde_json::to_vec(
                &json!({"choices":[{"message":{"content":serde_json::to_string(&next).unwrap()}}]}),
            )
            .unwrap(),
        )
        .unwrap();
        let destination = dir.path().join(format!("{name}.json"));
        let result = Command::new(env!("CARGO_BIN_EXE_skywrite"))
            .env_remove("OPENROUTER_API_KEY")
            .env("SKYWRITE_HOME", dir.path().join("personal"))
            .args(["edit", "suggest", "--analysis"])
            .arg(&report_path)
            .arg("--transcript")
            .arg(&transcript_path)
            .args([
                "--brief",
                brief,
                "--model",
                "fixture/dialogue",
                "--cache-dir",
            ])
            .arg(dir.path().join("cache"))
            .arg("--output")
            .arg(&destination)
            .output()
            .unwrap();
        assert_eq!(
            result.status.success(),
            accepted,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(destination.exists(), accepted);
        if !accepted {
            assert!(
                String::from_utf8_lossy(&result.stderr)
                    .contains("outside the provided analyses/transcripts")
            );
        }
    }
    assert_eq!(std::fs::read(&source).unwrap(), source_before);
    assert_eq!(std::fs::read(&transcript_path).unwrap(), transcript_before);
    assert_eq!(std::fs::read(&report_path).unwrap(), report_before);
    assert_eq!(std::fs::read(&subtitle).unwrap(), subtitle_bytes);
}
