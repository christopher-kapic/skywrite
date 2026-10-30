//! Tests at the real FFmpeg boundary, not merely serialized command strings.
use skywrite::{
    export, media, process,
    timeline::{self, AudioClip, Clip, Edit, Rational},
};
use std::{ffi::OsString, fs, path::Path, time::Duration};
fn command(program: &str, args: &[&str]) -> std::process::Output {
    process::run(
        program,
        &process::args(args),
        Duration::from_secs(60),
        4 * 1024 * 1024,
    )
    .unwrap()
}
fn make_fixtures(dir: &Path) {
    let switched = dir.join("speaker's $(touch HACKED) & <take>.mp4");
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
            "color=red:s=160x90:r=30:d=1.5",
            "-f",
            "lavfi",
            "-i",
            "color=blue:s=160x90:r=30:d=1.5",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=3",
            "-filter_complex_threads",
            "1",
            "-filter_complex",
            "[0:v][1:v]concat=n=2:v=1:a=0[v]",
            "-map",
            "[v]",
            "-map",
            "2:a",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-threads",
            "1",
            "-c:a",
            "aac",
            switched.to_str().unwrap(),
        ],
    );
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
            "color=green:s=90x160:r=25:d=3",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-threads",
            "1",
            dir.join("green.mp4").to_str().unwrap(),
        ],
    );
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
            "color=yellow:s=160x90",
            "-frames:v",
            "1",
            "-threads",
            "1",
            dir.join("title.png").to_str().unwrap(),
        ],
    );
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
            "sine=frequency=880:sample_rate=44100:duration=2",
            dir.join("voice.wav").to_str().unwrap(),
        ],
    );
}
fn edit() -> Edit {
    Edit {
        version: 1,
        name: "A 'quoted' & <edit>".into(),
        width: 160,
        height: 90,
        fps: Rational { num: 30, den: 1 },
        clips: vec![
            Clip {
                source: Some("speaker's $(touch HACKED) & <take>.mp4".into()),
                source_in: 1.25,
                duration: 0.5,
                speed: 2.0,
                volume: 0.0,
            },
            Clip {
                source: None,
                source_in: 0.0,
                duration: 0.2,
                speed: 1.0,
                volume: 1.0,
            },
            Clip {
                source: Some("green.mp4".into()),
                source_in: 0.0,
                duration: 0.5,
                speed: 0.5,
                volume: 1.0,
            },
            Clip {
                source: Some("title.png".into()),
                source_in: 0.0,
                duration: 0.3,
                speed: 1.0,
                volume: 1.0,
            },
        ],
        audio: vec![AudioClip {
            source: "voice.wav".into(),
            source_in: 0.1,
            offset: 0.7,
            duration: 0.5,
            speed: 1.01,
            volume: 0.7,
        }],
    }
}
fn center(path: &Path, time: &str) -> [u8; 3] {
    let result = command(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-nostdin",
            "-ss",
            time,
            "-i",
            path.to_str().unwrap(),
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-pix_fmt",
            "rgb24",
            "-threads",
            "1",
            "-f",
            "rawvideo",
            "pipe:1",
        ],
    );
    let index = (45 * 160 + 80) * 3;
    result.stdout[index..index + 3].try_into().unwrap()
}
fn rms(path: &Path, start: &str, duration: &str) -> f64 {
    let bytes = command(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-nostdin",
            "-ss",
            start,
            "-t",
            duration,
            "-i",
            path.to_str().unwrap(),
            "-map",
            "0:a:0",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-f",
            "f32le",
            "pipe:1",
        ],
    )
    .stdout;
    let samples = bytes
        .chunks_exact(4)
        .map(|v| f32::from_le_bytes(v.try_into().unwrap()) as f64)
        .collect::<Vec<_>>();
    (samples.iter().map(|s| s * s).sum::<f64>() / samples.len() as f64).sqrt()
}

#[test]
fn real_render_preserves_timing_picture_and_audio() {
    let dir = tempfile::tempdir().unwrap();
    make_fixtures(dir.path());
    let validated = timeline::validate(&edit(), dir.path()).unwrap();
    let output = dir.path().join("render.mp4");
    export::render(&validated, &output, false).unwrap();
    let info = media::probe(&output).unwrap();
    assert!(info.has_audio() && info.has_video());
    assert!((info.duration - 1.5).abs() < 0.04);
    let frame_count = command(
        "ffprobe",
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
            output.to_str().unwrap(),
        ],
    );
    assert_eq!(String::from_utf8(frame_count.stdout).unwrap().trim(), "45");
    let red = center(&output, "0.05");
    assert!(red[0] > 200 && red[2] < 20, "{red:?}");
    let blue = center(&output, "0.35");
    assert!(
        blue[2] > 200 && blue[0] < 20,
        "retimed trim not blue: {blue:?}"
    );
    let black = center(&output, "0.6");
    assert!(black.iter().all(|v| *v < 10), "gap not black: {black:?}");
    let green = center(&output, "0.9");
    assert!(green[1] > 80 && green[0] < 20, "{green:?}");
    let yellow = center(&output, "1.35");
    assert!(
        yellow[0] > 200 && yellow[1] > 200 && yellow[2] < 20,
        "still not yellow: {yellow:?}"
    );
    assert!(
        rms(&output, "0.1", "0.3") < 0.0001,
        "muted primary audio leaked"
    );
    assert!(
        rms(&output, "0.8", "0.2") > 0.01,
        "offset external audio missing"
    );
    let bytes = fs::read(&output).unwrap();
    assert!(export::render(&validated, &output, false).is_err());
    assert_eq!(fs::read(&output).unwrap(), bytes);
    assert!(export::render(&validated, &dir.path().join("green.mp4"), true).is_err());
    assert!(!dir.path().join("HACKED").exists());
}

#[test]
fn interchange_and_python_export_work_with_hostile_names() {
    let dir = tempfile::tempdir().unwrap();
    make_fixtures(dir.path());
    let validated = timeline::validate(&edit(), dir.path()).unwrap();
    let fcpxml = dir.path().join("edit.fcpxml");
    export::export(&validated, "fcpxml", &fcpxml, false).unwrap();
    let check = r#"import sys,xml.etree.ElementTree as E
r=E.parse(sys.argv[1]).getroot()
assert r.attrib['version']=='1.10'
assert r.find('.//project').attrib['name']=="A 'quoted' & <edit>"
assert r.find('.//sequence').attrib['duration']=='3/2s'
assert len(r.findall('.//timeMap'))==3
assert r.find('.//video') is not None
assert r.find('.//asset-clip[@srcEnable="video"]') is not None
assert r.find('.//asset-clip[@srcEnable="audio"]') is not None
assert all('file://' in x.attrib['src'] for x in r.findall('.//media-rep'))
"#;
    command("python3", &["-c", check, fcpxml.to_str().unwrap()]);
    let script = dir.path().join("render.py");
    export::export(&validated, "ffmpeg", &script, false).unwrap();
    let output = dir.path().join("python.mp4");
    command(
        "python3",
        &[
            script.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ],
    );
    assert!((media::probe(&output).unwrap().duration - 1.5).abs() < 0.04);
    let blue = center(&output, "0.35");
    assert!(blue[2] > 200 && blue[0] < 20);
    let args: Vec<OsString> = vec![
        script.into_os_string(),
        "--output".into(),
        output.clone().into_os_string(),
    ];
    assert!(process::run("python3", &args, Duration::from_secs(5), 1024 * 1024).is_err());
    assert!(!dir.path().join("HACKED").exists());
    for target in ["hyperframes", "remotion"] {
        let project = dir.path().join(target);
        export::export(&validated, target, &project, false).unwrap();
        assert!(
            project
                .join(if target == "hyperframes" {
                    "index.html"
                } else {
                    "index.jsx"
                })
                .is_file()
        );
        assert!(export::export(&validated, target, &project, true).is_err());
    }
}

#[test]
fn delayed_stream_presentation_times_survive_trim_and_retime() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("delayed.mkv");
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
            "-itsoffset",
            "0.5",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=2.5",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-c:a",
            "pcm_s16le",
            input.to_str().unwrap(),
        ],
    );
    for (index, (source_in, speed, duration)) in [
        (0.0, 1.0, 1.0),
        (0.25, 1.0, 1.0),
        (0.75, 1.0, 1.0),
        (0.0, 2.0, 1.0),
        (0.0, 0.5, 1.5),
        (0.0, 1.0, 0.25),
    ]
    .into_iter()
    .enumerate()
    {
        let mut specification = edit();
        specification.audio.clear();
        specification.clips = vec![Clip {
            source: Some(input.clone()),
            source_in,
            duration,
            speed,
            volume: 1.0,
        }];
        let validated = timeline::validate(&specification, dir.path()).unwrap();
        let output = dir.path().join(format!("delayed-{index}.mp4"));
        export::render(&validated, &output, false).unwrap();
        let onset = ((0.5 - source_in) / speed).max(0.0);
        if onset > 0.1 {
            assert!(
                rms(&output, "0.02", &(onset.min(duration) - 0.05).to_string()) < 0.0001,
                "audio incorrectly shifted earlier for source_in={source_in}, speed={speed}"
            );
        }
        if onset + 0.15 < duration {
            assert!(
                rms(&output, &(onset + 0.1).to_string(), "0.1") > 0.03,
                "audio missing after expected onset"
            );
        }
    }
}
