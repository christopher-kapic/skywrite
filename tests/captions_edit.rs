//! Captions generated from source transcripts follow the edit's cuts.
use serde_json::json;
use skywrite::{export, timeline};
use std::process::Command;

#[test]
fn transcript_captions_follow_cuts_mutes_and_exports() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("talk.mp4");
    assert!(
        Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=red:s=160x90:r=30:d=6",
                "-f",
                "lavfi",
                "-i",
                "sine=d=6",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest"
            ])
            .arg(&video)
            .status()
            .unwrap()
            .success()
    );
    let words = ["one", "two", "three", "four", "five"]
        .iter()
        .enumerate()
        .map(|(i, w)| json!({"text":w,"start":0.2 + i as f64,"end":0.4 + i as f64}))
        .collect::<Vec<_>>();
    std::fs::write(
        dir.path().join("talk.json"),
        serde_json::to_vec(&json!({"version":1,"source":"talk.mp4","duration":6.0,"text":"one two three four five","words":words})).unwrap(),
    )
    .unwrap();
    let edit = dir.path().join("edit.json");
    std::fs::write(
        &edit,
        serde_json::to_vec(
            &json!({"version":2,"name":"Captions","width":160,"height":90,"fps":{"num":30,"den":1},
        "video":[{"clips":[
            {"start":0,"source":"talk.mp4","source_in":0,"duration":1.5},
            {"start":1.5,"source":"talk.mp4","source_in":3,"duration":1.5},
            {"start":3,"source":"talk.mp4","source_in":2,"duration":1,"audio":{"volume":0}}
        ]}],
        "captions":[{"from_transcripts":["talk.json"],"language":"eng"}]}),
        )
        .unwrap(),
    )
    .unwrap();
    let validated = timeline::load_and_validate(&edit).unwrap();

    let srt = dir.path().join("out.srt");
    let out = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(["edit", "captions"])
        .arg(&edit)
        .arg("-o")
        .arg(&srt)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(&srt).unwrap();
    assert!(text.contains("one two"), "{text}");
    assert!(text.contains("four five"), "{text}");
    assert!(
        !text.contains("three"),
        "muted or cut words must not be captioned: {text}"
    );
    // "four" (source 3.2s) plays at 1.5 + 0.2 = 1.7s on the output timeline.
    assert!(text.contains("00:00:01,700"), "{text}");

    let xml = dir.path().join("premiere.xml");
    let result = export::export(&validated, "premiere", &xml, false).unwrap();
    let sidecar = dir.path().join("premiere.eng.srt");
    assert_eq!(result["caption_files"][0], json!(sidecar));
    assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), text);
    // Sidecars follow the same no-clobber rule as the export.
    assert!(
        export::export(
            &validated,
            "premiere",
            &dir.path().join("premiere2.xml"),
            false
        )
        .is_ok()
    );
    std::fs::remove_file(dir.path().join("premiere2.xml")).unwrap();
    assert!(
        export::export(
            &validated,
            "premiere",
            &dir.path().join("premiere2.xml"),
            false
        )
        .is_err()
    );

    let fcpxml = dir.path().join("edit.fcpxml");
    export::export(&validated, "fcpxml", &fcpxml, false).unwrap();
    let body = std::fs::read_to_string(&fcpxml).unwrap();
    assert_eq!(body.matches("<caption lane=").count(), 2);
    assert!(body.contains(">four five<"));

    let bad = dir.path().join("bad.json");
    std::fs::write(
        &bad,
        serde_json::to_vec(&json!({"version":2,"duration":1,
        "captions":[{"source":"x.srt","from_transcripts":["talk.json"]}]}))
        .unwrap(),
    )
    .unwrap();
    assert!(timeline::load_and_validate(&bad).is_err());
}
