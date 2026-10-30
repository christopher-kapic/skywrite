//! Real cached CLI failure must stop before later chunks and preserve outputs.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;
use std::process::Command;

#[test]
fn excessive_cached_response_stops_cli_and_preserves_source_and_output() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.wav");
    assert!(
        Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=16000:cl=mono",
                "-t",
                "30",
                "-c:a",
                "pcm_s16le"
            ])
            .arg(&source)
            .status()
            .unwrap()
            .success()
    );
    let original_source = std::fs::read(&source).unwrap();
    let info = skywrite::media::probe(&source).unwrap();
    let chunk = dir.path().join("chunk.wav");
    skywrite::media::extract_audio(&info, &chunk, 0., 10.).unwrap();
    let request = json!({"model":"budget-fixture", "input_audio":{"data":STANDARD.encode(std::fs::read(&chunk).unwrap()),"format":"wav"},"response_format":"verbose_json","timestamp_granularities":["word"]});
    let cache = dir.path().join("cache");
    std::fs::create_dir(&cache).unwrap();
    let hash = skywrite::openrouter::cache_key(&[
        b"https://openrouter.ai/api/v1",
        b"audio/transcriptions",
        &serde_json::to_vec(&request).unwrap(),
    ]);
    // Sparse, owned file: first excess byte of the real 8MiB response/cache cap.
    std::fs::File::create(cache.join(format!("{hash}.json")))
        .unwrap()
        .set_len(8 * 1024 * 1024 + 1)
        .unwrap();
    for existing in [false, true] {
        let output_path = dir.path().join(if existing {
            "existing.json"
        } else {
            "absent.json"
        });
        if existing {
            std::fs::write(&output_path, b"original output").unwrap();
        }
        let output = Command::new(env!("CARGO_BIN_EXE_skywrite"))
            .env_remove("OPENROUTER_API_KEY")
            .arg("transcribe")
            .arg(&source)
            .args([
                "--model",
                "budget-fixture",
                "--chunk-seconds",
                "10",
                "--cache-dir",
            ])
            .arg(&cache)
            .arg("--output")
            .arg(&output_path)
            .arg("--overwrite")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let diagnostic = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            diagnostic.contains("response exceeded 8 MiB"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("chunk 1/3"));
        assert!(!diagnostic.contains("chunk 2/3"));
        if existing {
            assert_eq!(std::fs::read(&output_path).unwrap(), b"original output");
        } else {
            assert!(!output_path.exists());
        }
        assert_eq!(std::fs::read(&source).unwrap(), original_source);
    }
}
