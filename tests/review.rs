//! Review aids against real decoded media.
use serde_json::{Value, json};
use std::{path::Path, process::Command};

fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn skywrite(args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn fixtures(dir: &Path) {
    let p = |n: &str| dir.join(n).to_str().unwrap().to_owned();
    // "Speaker": picture plus a quiet tone standing in for speech.
    ffmpeg(&[
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
        "sine=frequency=300:sample_rate=48000:duration=6",
        "-af",
        "volume=0.8",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-shortest",
        &p("talk.mp4"),
    ]);
    ffmpeg(&[
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "color=blue:s=160x90:r=30:d=6",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        &p("broll.mp4"),
    ]);
    // Loud continuous noise, like applause.
    ffmpeg(&[
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "anoisesrc=d=6:c=pink:a=0.5:r=48000",
        &p("applause.wav"),
    ]);
    // A loud music bed.
    ffmpeg(&[
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=500:sample_rate=48000:duration=6",
        "-af",
        "volume=7",
        &p("bed.wav"),
    ]);
    let transcript = json!({"version":1,"source":"talk.mp4","duration":6.0,"text":"we came to Evanston Evanston today",
        "words":[{"text":"we","start":0.5,"end":0.7},{"text":"came","start":0.8,"end":1.0},{"text":"to","start":1.1,"end":1.2},
                 {"text":"Evanston","start":1.3,"end":1.7},{"text":"Evanston,","start":1.8,"end":2.2},{"text":"um","start":2.4,"end":2.6},
                 {"text":"today","start":2.8,"end":3.2}]});
    std::fs::write(
        dir.join("talk.transcript.json"),
        serde_json::to_vec(&transcript).unwrap(),
    )
    .unwrap();
}

fn write_edit(dir: &Path, name: &str, edit: Value) -> String {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_vec(&edit).unwrap()).unwrap();
    path.to_str().unwrap().to_owned()
}

fn kinds(report: &Value) -> Vec<String> {
    report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["kind"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn lint_reports_objective_problems_and_stays_quiet_otherwise() {
    let dir = tempfile::tempdir().unwrap();
    fixtures(dir.path());
    let transcript = dir.path().join("talk.transcript.json");
    let transcript = transcript.to_str().unwrap();
    let base = |video: Value, audio: Value| json!({"version":2,"width":160,"height":90,"fps":{"num":30,"den":1},"video":video,"audio":audio});

    // Music over a muted, visibly speaking presenter (Ralph v1b's opening).
    let silent = write_edit(
        dir.path(),
        "silent.json",
        base(
            json!([{"clips":[{"start":0,"source":"talk.mp4","duration":3.5,"audio":{"volume":0}}]}]),
            json!([{"clips":[{"start":0,"source":"bed.wav","duration":3.5,"volume":0.2}]}]),
        ),
    );
    let report = skywrite(&[
        "review",
        "lint",
        &silent,
        "--transcript",
        transcript,
        "--no-levels",
    ]);
    assert!(
        kinds(&report).contains(&"visible_speaker_without_dialogue".to_owned()),
        "{report}"
    );

    // Covering the speaker with opaque B-roll for the whole span removes it.
    let covered = write_edit(
        dir.path(),
        "covered.json",
        base(
            json!([{"clips":[{"start":0,"source":"talk.mp4","duration":3.5,"audio":{"volume":0}}]},
               {"clips":[{"start":0,"source":"broll.mp4","duration":3.5,"audio":{"volume":0}}]}]),
            json!([{"clips":[{"start":0,"source":"bed.wav","duration":3.5,"volume":0.2}]}]),
        ),
    );
    let report = skywrite(&[
        "review",
        "lint",
        &covered,
        "--transcript",
        transcript,
        "--no-levels",
    ]);
    assert!(
        !kinds(&report).contains(&"visible_speaker_without_dialogue".to_owned()),
        "{report}"
    );

    // Audible dialogue with a stumble and a filler.
    let talking = write_edit(
        dir.path(),
        "talking.json",
        base(
            json!([{"clips":[{"start":0,"source":"talk.mp4","duration":3.5}]}]),
            json!([]),
        ),
    );
    let report = skywrite(&[
        "review",
        "lint",
        &talking,
        "--transcript",
        transcript,
        "--no-levels",
    ]);
    let found = kinds(&report);
    assert!(
        found.contains(&"repeated_word_in_select".to_owned()),
        "{report}"
    );
    assert!(found.contains(&"filler_words".to_owned()), "{report}");
    assert!(
        !found.contains(&"visible_speaker_without_dialogue".to_owned()),
        "{report}"
    );

    // A repeat joined by a cut is reported as such.
    let joined = write_edit(
        dir.path(),
        "joined.json",
        base(
            json!([{"clips":[{"start":0,"source":"talk.mp4","source_in":0.4,"duration":1.4},
                         {"start":1.4,"source":"talk.mp4","source_in":1.75,"duration":0.5}]}]),
            json!([]),
        ),
    );
    let report = skywrite(&[
        "review",
        "lint",
        &joined,
        "--transcript",
        transcript,
        "--no-levels",
    ]);
    assert!(
        kinds(&report).contains(&"repeated_word_across_cut".to_owned()),
        "{report}"
    );

    // A cut inside a word is flagged; a straight split through the same word is not.
    let clipped = write_edit(
        dir.path(),
        "clipped.json",
        base(
            json!([{"clips":[{"start":0,"source":"talk.mp4","source_in":0.4,"duration":1.1},
                         {"start":1.1,"source":"talk.mp4","source_in":2.7,"duration":0.6}]}]),
            json!([]),
        ),
    );
    let report = skywrite(&[
        "review",
        "lint",
        &clipped,
        "--transcript",
        transcript,
        "--no-levels",
    ]);
    let items: Vec<&Value> = report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["kind"] == "word_clipped_at_cut")
        .flat_map(|w| w["items"].as_array().unwrap())
        .collect();
    assert_eq!(items.len(), 1, "{report}");
    assert_eq!(items[0]["word"], "Evanston", "{report}");
    assert_eq!(items[0]["edge"], "out", "{report}");
    let split = write_edit(
        dir.path(),
        "split.json",
        base(
            json!([{"clips":[{"start":0,"source":"talk.mp4","source_in":0.4,"duration":1.1},
                         {"start":1.1,"source":"talk.mp4","source_in":1.5,"duration":0.9}]}]),
            json!([]),
        ),
    );
    let report = skywrite(&[
        "review",
        "lint",
        &split,
        "--transcript",
        transcript,
        "--no-levels",
    ]);
    assert!(
        !kinds(&report).contains(&"word_clipped_at_cut".to_owned()),
        "{report}"
    );

    // Applause cut off abruptly, and a bed as loud as the dialogue.
    let mix = write_edit(
        dir.path(),
        "mix.json",
        base(
            json!([{"clips":[{"start":0,"source":"talk.mp4","duration":3.5,"audio":{"fade_out":0.3}}]}]),
            json!([{"clips":[{"start":3.3,"source":"applause.wav","duration":1}]},
               {"clips":[{"start":0,"source":"bed.wav","duration":3.5,"fade_out":0.3}]}]),
        ),
    );
    let report = skywrite(&["review", "lint", &mix, "--transcript", transcript]);
    let found = kinds(&report);
    assert!(found.contains(&"sound_cut_mid_flow".to_owned()), "{report}");
    assert!(
        found.contains(&"bed_close_to_dialogue".to_owned()),
        "{report}"
    );

    // A faded release and a ducked bed are not flagged.
    let fixed = write_edit(
        dir.path(),
        "fixed.json",
        base(
            json!([{"clips":[{"start":0,"source":"talk.mp4","duration":3.5,"audio":{"fade_out":0.3}}]}]),
            json!([{"clips":[{"start":3.3,"source":"applause.wav","duration":1,"fade_out":0.8}]},
               {"clips":[{"start":0,"source":"bed.wav","duration":3.5,"volume":0.01}]}]),
        ),
    );
    let report = skywrite(&["review", "lint", &fixed, "--transcript", transcript]);
    let found = kinds(&report);
    assert!(
        !found.contains(&"sound_cut_mid_flow".to_owned()),
        "{report}"
    );
    assert!(
        !found.contains(&"bed_close_to_dialogue".to_owned()),
        "{report}"
    );
}

#[test]
fn cut_auditions_cover_every_cut() {
    let dir = tempfile::tempdir().unwrap();
    fixtures(dir.path());
    let edit = write_edit(
        dir.path(),
        "edit.json",
        json!({"version":2,"width":160,"height":90,"fps":{"num":30,"den":1},
        "video":[{"clips":[{"start":0,"source":"talk.mp4","duration":2},{"start":2,"source":"talk.mp4","source_in":3,"duration":2}]},
                 {"clips":[{"start":1,"source":"broll.mp4","duration":0.5,"audio":{"volume":0}}]}]}),
    );
    let out = dir.path().join("cuts");
    let report = skywrite(&[
        "review",
        "cuts",
        &edit,
        "--handles",
        "0.5",
        "--output-dir",
        out.to_str().unwrap(),
    ]);
    assert_eq!(report["cut_count"], 3);
    for cut in report["cuts"].as_array().unwrap() {
        let clip = cut["clip"].as_str().unwrap();
        let info = skywrite::media::probe(Path::new(clip)).unwrap();
        assert!(info.has_audio() && info.has_video());
        assert!(
            info.duration > 0.8 && info.duration < 1.1,
            "{}",
            info.duration
        );
    }
    // The directory must be new.
    let again = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args([
            "review",
            "cuts",
            &edit,
            "--output-dir",
            out.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!again.status.success());
    // No temporary render is left behind.
    assert!(
        std::fs::read_dir(&out).unwrap().all(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("cut-"))
    );
}

#[test]
fn contact_sheets_use_exact_consecutive_frames() {
    let dir = tempfile::tempdir().unwrap();
    fixtures(dir.path());
    let sheet = dir.path().join("sheet.jpg");
    let report = skywrite(&[
        "review",
        "sheet",
        dir.path().join("talk.mp4").to_str().unwrap(),
        "--start",
        "1",
        "--duration",
        "0.5",
        "--every",
        "2",
        "-o",
        sheet.to_str().unwrap(),
    ]);
    let tiles = report["tiles"].as_array().unwrap();
    assert_eq!(tiles.len(), 8);
    assert_eq!(tiles[0]["frame"], 30);
    assert_eq!(tiles[1]["frame"], 32);
    assert!((tiles[1]["source_time"].as_f64().unwrap() - (1.0 + 2.0 / 30.0)).abs() < 0.002);
    assert!(sheet.is_file());
    let too_many = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args([
            "review",
            "sheet",
            dir.path().join("talk.mp4").to_str().unwrap(),
            "--duration",
            "5",
            "-o",
            dir.path().join("big.jpg").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!too_many.status.success());
}
