//! NLE timelines come back into edits: Premiere XML round trip and .prproj reading.
use serde_json::{Value, json};
use std::{io::Write, process::Command};

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

#[test]
fn premiere_xml_round_trips_layers_speed_fades_and_levels() {
    let dir = tempfile::tempdir().unwrap();
    let p = |n: &str| dir.path().join(n).to_str().unwrap().to_owned();
    for (name, filter) in [
        ("a.mp4", "color=red:s=160x90:r=30:d=8"),
        ("b.mp4", "color=blue:s=160x90:r=30:d=8"),
    ] {
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    filter,
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=d=8",
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-shortest",
                    &p(name)
                ])
                .status()
                .unwrap()
                .success()
        );
    }
    assert!(
        Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=d=8",
                &p("tone.wav")
            ])
            .status()
            .unwrap()
            .success()
    );
    let edit = json!({"version":2,"name":"Round trip","width":160,"height":90,"fps":{"num":30,"den":1},
        "video":[{"clips":[{"start":0,"source":"a.mp4","source_in":1,"duration":2,"audio":{"volume":0.5}},
                           {"start":2,"source":"a.mp4","source_in":4,"duration":1.5,"speed":1.2}]},
                 {"clips":[{"start":0.5,"source":"b.mp4","source_in":2,"duration":1,"fade_in":0.5,"opacity":0.8,"audio":{"volume":0}}]}],
        "audio":[{"clips":[{"start":1,"source":"tone.wav","source_in":0.5,"duration":2,
            "volume_keyframes":[{"time":0,"volume":1},{"time":1,"volume":0.25}]}]}],
        "chapters":[{"start":0,"title":"Start"}]});
    std::fs::write(p("edit.json"), serde_json::to_vec(&edit).unwrap()).unwrap();
    skywrite(&[
        "edit",
        "export",
        &p("edit.json"),
        "--to",
        "premiere",
        "-o",
        &p("edit.xml"),
    ]);
    let report = skywrite(&["edit", "import", &p("edit.xml"), "-o", &p("back.json")]);
    assert_eq!(report["validation"]["valid"], true, "{report}");
    let back: Value = serde_json::from_slice(&std::fs::read(p("back.json")).unwrap()).unwrap();
    let v1 = &back["video"][0]["clips"];
    assert_eq!(v1[0]["start"], 0.0);
    assert_eq!(v1[0]["source_in"], 1.0);
    assert_eq!(v1[0]["audio"]["volume"], 0.5);
    assert!((v1[1]["speed"].as_f64().unwrap() - 1.2).abs() < 1e-9);
    assert!((v1[1]["source_in"].as_f64().unwrap() - 4.0).abs() < 0.05);
    let v2 = &back["video"][1]["clips"][0];
    assert_eq!(v2["fade_in"], 0.5);
    assert_eq!(v2["opacity"], 0.8);
    assert_eq!(v2["audio"]["volume"], 0);
    let tone = &back["audio"][0]["clips"][0];
    assert_eq!(tone["start"], 1.0);
    assert_eq!(tone["volume_keyframes"][1]["volume"], 0.25);
    assert_eq!(back["chapters"][0]["title"], "Start");
    assert!(
        back["video"][0]["clips"][0]["source"]
            .as_str()
            .unwrap()
            .ends_with("a.mp4")
    );
}

#[test]
fn premiere_projects_are_read_experimentally_and_bad_input_fails() {
    let dir = tempfile::tempdir().unwrap();
    let ticks = |seconds: f64| (seconds * 254_016_000_000.0) as u64;
    // Structure as described by a tester's working parser (object graph by IDs).
    let project = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<PremiereData Version="3">
  <VideoClipTrack ObjectID="10"><ClipTrack><ClipItems><TrackItems><TrackItem Index="0" ObjectRef="20"/></TrackItems></ClipItems></ClipTrack></VideoClipTrack>
  <VideoClipTrackItem ObjectID="20"><ClipTrackItem><TrackItem><Start>{}</Start><End>{}</End></TrackItem><SubClip ObjectRef="30"/></ClipTrackItem></VideoClipTrackItem>
  <SubClip ObjectID="30"><Clip ObjectRef="40"/><MasterClip ObjectURef="m-1"/></SubClip>
  <VideoClip ObjectID="40"><Clip><Source ObjectRef="50"/><InPoint>{}</InPoint><OutPoint>{}</OutPoint></Clip></VideoClip>
  <VideoMediaSource ObjectID="50"><MediaSource><Media ObjectURef="media-1"/></MediaSource></VideoMediaSource>
  <MasterClip ObjectUID="m-1"><Name>camera</Name></MasterClip>
  <Media ObjectUID="media-1"><ActualMediaFilePath>/footage/camera &amp; co.mxf</ActualMediaFilePath></Media>
</PremiereData>"#,
        ticks(1.0),
        ticks(3.0),
        ticks(2.0),
        ticks(4.0)
    );
    let path = dir.path().join("cut.prproj");
    let mut gz = flate2::write::GzEncoder::new(
        std::fs::File::create(&path).unwrap(),
        flate2::Compression::default(),
    );
    gz.write_all(project.as_bytes()).unwrap();
    gz.finish().unwrap();
    let out = dir.path().join("cut.json");
    let report = skywrite(&[
        "edit",
        "import",
        path.to_str().unwrap(),
        "-o",
        out.to_str().unwrap(),
    ]);
    assert!(
        report["notes"][0]
            .as_str()
            .unwrap()
            .starts_with("Experimental")
    );
    assert_eq!(report["validation"]["valid"], false); // media is on another machine
    let edit: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let clip = &edit["video"][0]["clips"][0];
    assert_eq!(clip["source"], "/footage/camera & co.mxf");
    assert_eq!(clip["start"], 1.0);
    assert_eq!(clip["duration"], 2.0);
    assert_eq!(clip["source_in"], 2.0);

    std::fs::write(dir.path().join("x.xml"), "<fcpxml version=\"1.10\"/>").unwrap();
    let rejected = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .args([
            "edit",
            "import",
            dir.path().join("x.xml").to_str().unwrap(),
            "-o",
            dir.path().join("y.json").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
}
