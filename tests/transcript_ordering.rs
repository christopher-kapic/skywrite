//! Provider protocol regression: concurrent words and chunk tolerance must not
//! corrupt native chronology or alter provider timestamps.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;
use std::process::Command;

#[test]
fn cached_provider_words_and_chunk_boundaries_keep_source_timestamps() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.wav");
    let status = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-n",
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=0.1*sin(2*PI*(317*t+2*t*t)):s=16000:d=22",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&source)
        .status()
        .unwrap();
    assert!(status.success());
    let info = skywrite::media::probe(&source).unwrap();
    let cache = temp.path().join("cache");
    std::fs::create_dir_all(cache.join("responses")).unwrap();
    let responses = [
        json!({"text":"late early repeat repeat", "words":[
            {"word":"late","start":10.1,"end":10.14},
            {"word":"early","start":0.2,"end":0.9},
            {"word":"repeat","start":0.2,"end":0.8},
            {"word":"repeat","start":0.2,"end":0.7}
        ]}),
        json!({"text":"next", "words":[{"word":"next","start":0.0,"end":0.5}]}),
    ];
    for (index, offset) in [1.0, 11.0].into_iter().enumerate() {
        let audio = temp.path().join(format!("chunk-{index}.wav"));
        skywrite::media::extract_audio(&info, &audio, offset, 10.0).unwrap();
        let request = json!({"model":"provider-order-fixture","input_audio":{"data":STANDARD.encode(std::fs::read(audio).unwrap()),"format":"wav"},"response_format":"verbose_json","timestamp_granularities":["word"]});
        let key = skywrite::openrouter::cache_key(&[
            b"https://openrouter.ai/api/v1",
            b"audio/transcriptions",
            &serde_json::to_vec(&request).unwrap(),
        ]);
        std::fs::write(
            cache.join("responses").join(format!("{key}.json")),
            serde_json::to_vec(&responses[index]).unwrap(),
        )
        .unwrap();
    }
    let output_path = temp.path().join("transcript.json");
    let output = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .env_remove("OPENROUTER_API_KEY")
        .arg("transcribe")
        .arg(&source)
        .args([
            "--model",
            "provider-order-fixture",
            "--start",
            "1",
            "--duration",
            "20",
            "--chunk-seconds",
            "10",
            "--cache-dir",
        ])
        .arg(&cache)
        .arg("--output")
        .arg(&output_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let transcript = skywrite::transcript::Transcript::load(&output_path).unwrap();
    assert_eq!(transcript.text, "late early repeat repeat next");
    assert_eq!(transcript.words.len(), 5);
    let actual: Vec<_> = transcript
        .words
        .iter()
        .map(|w| (w.text.as_str(), w.start, w.end))
        .collect();
    assert_eq!(
        actual,
        vec![
            ("early", 1.2, 1.9),
            ("repeat", 1.2, 1.8),
            ("repeat", 1.2, 1.7),
            ("next", 11.0, 11.5),
            ("late", 11.1, 11.14)
        ]
    );
    assert_eq!(result["usage"]["cache_hits"], 2);
    assert_eq!(
        result["usage"]["word_ordering"]["provider_reordered_chunks"],
        1
    );
    assert_eq!(
        result["usage"]["word_ordering"]["chunk_boundary_reordered"],
        true
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("reordered provider words chronologically"));
    assert!(stderr.contains("reordered words across chunk boundaries"));
}
