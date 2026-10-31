//! Container metadata and actual decoding agree on source-relative time.
use skywrite::{
    export, media, process,
    timeline::{self, Clip, Edit, Rational},
};
use std::{path::Path, time::Duration};
fn command(program: &str, args: &[&str]) -> std::process::Output {
    process::run(
        program,
        &process::args(args),
        Duration::from_secs(60),
        4 * 1024 * 1024,
    )
    .unwrap()
}
fn specification(source: &Path, source_in: f64, duration: f64) -> Edit {
    Edit {
        version: 1,
        name: "Origin".into(),
        width: 160,
        height: 90,
        fps: Rational { num: 30, den: 1 },
        clips: vec![Clip {
            source: Some(source.to_owned()),
            source_in,
            duration,
            speed: 1.0,
            volume: 1.0,
        }],
        audio: vec![],
        captions: vec![],
        chapters: vec![],
    }
}
#[test]
fn presentation_duration_normalizes_only_matroska_end_timestamps() {
    let dir = tempfile::tempdir().unwrap();
    for extension in ["mkv", "mp4", "ts"] {
        for origin in [-1, 0, 5] {
            let source = dir.path().join(format!("origin-{origin}.{extension}"));
            command(
                "ffmpeg",
                &[
                    "-v",
                    "error",
                    "-nostdin",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=red:s=160x90:r=30:d=3",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=48000:duration=3",
                    "-vf",
                    &format!("setpts=PTS+({origin})/TB"),
                    "-af",
                    &format!("asetpts=PTS+({origin})/TB"),
                    "-c:v",
                    if extension == "mkv" {
                        "ffv1"
                    } else {
                        "libx264"
                    },
                    "-threads",
                    "1",
                    "-c:a",
                    if extension == "mkv" {
                        "pcm_s16le"
                    } else {
                        "aac"
                    },
                    "-fps_mode",
                    "passthrough",
                    "-avoid_negative_ts",
                    "disabled",
                    source.to_str().unwrap(),
                ],
            );
            let info = media::probe(&source).unwrap();
            // MOV edit lists discard the negative portion; TS rebases it.
            let expected = if extension == "mp4" && origin == -1 {
                2.0
            } else {
                3.0
            };
            assert!(
                (info.duration - expected).abs() < 0.04,
                "{extension} origin={origin}: {info:?}"
            );
            media::validate_range(&info, expected - 0.3, 0.2).unwrap();
            assert!(
                media::validate_range(&info, expected + 0.1, 0.2)
                    .unwrap_err()
                    .to_string()
                    .contains("exceeds media duration")
            );
            let valid =
                timeline::validate(&specification(&source, expected - 0.3, 0.2), dir.path())
                    .unwrap();
            let invalid =
                timeline::validate(&specification(&source, expected + 0.1, 0.2), dir.path())
                    .unwrap_err();
            assert!(
                invalid.to_string().contains("source range ends at"),
                "{invalid:#}"
            );
            let output = dir.path().join(format!("last-{origin}-{extension}.mp4"));
            export::render(&valid, &output, false)
                .unwrap_or_else(|error| panic!("{extension} origin={origin}: {error:#}"));
            assert!((media::probe(&output).unwrap().duration - 0.2).abs() < 0.05);
            if origin == 5 {
                let result = command(
                    env!("CARGO_BIN_EXE_skywrite"),
                    &[
                        "analyze",
                        source.to_str().unwrap(),
                        "--local",
                        "--frames",
                        "4",
                        "--frames-dir",
                        dir.path()
                            .join(format!("analysis-{extension}"))
                            .to_str()
                            .unwrap(),
                    ],
                );
                let analysis: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
                assert!(analysis.is_object());
                let audio = dir.path().join(format!("last-{extension}.wav"));
                media::extract_audio(&info, &audio, expected - 0.3, 0.2).unwrap();
                assert!(std::fs::metadata(audio).unwrap().len() > 1000);
            }
            let frames = dir.path().join(format!("frames-{origin}-{extension}"));
            assert_eq!(
                media::sample_frames(&info, &frames, 4, 0.0, info.duration)
                    .unwrap()
                    .len(),
                4
            );
            let xml = dir
                .path()
                .join(format!("origin-{origin}-{extension}.fcpxml"));
            export::export(&valid, "fcpxml", &xml, false).unwrap();
            let check = "import sys,xml.etree.ElementTree as E; from fractions import Fraction; a=E.parse(sys.argv[1]).find('.//asset'); assert abs(float(Fraction(a.attrib['duration'][:-1]))-float(sys.argv[2]))<0.04";
            command(
                "python3",
                &["-c", check, xml.to_str().unwrap(), &expected.to_string()],
            );
        }
    }
}
