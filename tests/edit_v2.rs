//! Layered edit format: compositing, finishing and interchange at real tool boundaries.
use serde_json::{Value, json};
use skywrite::{export, process, timeline};
use std::{path::Path, process::Command, time::Duration};

fn ffmpeg(args: &[&str]) -> Vec<u8> {
    process::run(
        "ffmpeg",
        &process::args(args),
        Duration::from_secs(60),
        16 * 1024 * 1024,
    )
    .unwrap()
    .stdout
}

fn fixtures(dir: &Path) {
    let p = |name: &str| dir.join(name).to_str().unwrap().to_owned();
    ffmpeg(&[
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
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-shortest",
        &p("red.mp4"),
    ]);
    // Narrow picture: letterboxing on an upper track must be transparent.
    ffmpeg(&[
        "-v",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "color=blue:s=80x90:r=30:d=3",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        &p("blue.mp4"),
    ]);
    ffmpeg(&[
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
        &p("yellow.png"),
    ]);
    ffmpeg(&[
        "-v",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=330:sample_rate=48000:duration=3",
        "-af",
        "volume=4",
        &p("tone.wav"),
    ]);
    // A 2x2x2 LUT that swaps red and blue; rows vary red fastest.
    let mut lut = String::from("TITLE \"swap\"\nLUT_3D_SIZE 2\n");
    for b in 0..2 {
        for g in 0..2 {
            for r in 0..2 {
                lut.push_str(&format!("{b} {g} {r}\n"));
            }
        }
    }
    std::fs::write(dir.join("swap.cube"), lut).unwrap();
}

fn pixel(path: &Path, time: f64, x: usize, y: usize) -> [u8; 3] {
    let raw = ffmpeg(&[
        "-v",
        "error",
        "-nostdin",
        "-ss",
        &time.to_string(),
        "-i",
        path.to_str().unwrap(),
        "-map",
        "0:v:0",
        "-frames:v",
        "1",
        "-pix_fmt",
        "rgb24",
        "-f",
        "rawvideo",
        "pipe:1",
    ]);
    let index = (y * 160 + x) * 3;
    raw[index..index + 3].try_into().unwrap()
}

fn samples(path: &Path, start: f64, duration: f64) -> Vec<f32> {
    ffmpeg(&[
        "-v",
        "error",
        "-nostdin",
        "-ss",
        &start.to_string(),
        "-t",
        &duration.to_string(),
        // Interleaved channels: a mono downmix would sum them and hide clipping.
        "-i",
        path.to_str().unwrap(),
        "-map",
        "0:a:0",
        "-ar",
        "48000",
        "-f",
        "f32le",
        "pipe:1",
    ])
    .chunks_exact(4)
    .map(|v| f32::from_le_bytes(v.try_into().unwrap()))
    .collect()
}
fn rms(values: &[f32]) -> f64 {
    (values.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
}

fn render(dir: &Path, edit: Value, name: &str) -> std::path::PathBuf {
    let validated = timeline::validate(&timeline::parse(edit).unwrap(), dir).unwrap();
    let output = dir.join(name);
    export::render(&validated, &output, false).unwrap();
    output
}

fn base(video: Value, audio: Value) -> Value {
    json!({"version":2,"name":"Layers","width":160,"height":90,"fps":{"num":30,"den":1},"video":video,"audio":audio})
}

#[test]
fn layers_composite_with_transparency_opacity_and_luts() {
    let dir = tempfile::tempdir().unwrap();
    fixtures(dir.path());
    let edit = base(
        json!([
            {"name":"A-roll","clips":[{"start":0,"source":"red.mp4","duration":3}]},
            {"name":"B-roll","clips":[
                {"start":1,"source":"blue.mp4","duration":1,"audio":{"volume":0}},
                {"start":2,"source":"yellow.png","duration":1,"opacity":0.5}
            ]}
        ]),
        json!([]),
    );
    let output = render(dir.path(), edit, "layers.mp4");
    let red = pixel(&output, 0.5, 80, 45);
    assert!(red[0] > 200 && red[2] < 30, "bottom layer: {red:?}");
    let blue = pixel(&output, 1.5, 80, 45);
    assert!(blue[2] > 200 && blue[0] < 30, "upper layer: {blue:?}");
    // The 80px-wide blue picture is centered; its letterbox shows red beneath.
    let edge = pixel(&output, 1.5, 5, 45);
    assert!(
        edge[0] > 200 && edge[2] < 30,
        "letterbox must be transparent: {edge:?}"
    );
    let blend = pixel(&output, 2.5, 80, 45);
    assert!(
        blend[0] > 200 && (90..170).contains(&blend[1]) && blend[2] < 40,
        "half-opacity yellow over red: {blend:?}"
    );
    // Linked sound from the bottom layer continues under the muted B-roll.
    assert!(rms(&samples(&output, 1.2, 0.5)) > 0.05);

    let graded = json!({"version":2,"width":160,"height":90,"fps":{"num":30,"den":1},
        "sources":[{"path":"red.mp4","lut":"swap.cube","color":{"transfer":"test-swap"}}],
        "video":[{"clips":[{"start":0,"source":"red.mp4","duration":1}]}]});
    let output = render(dir.path(), graded, "lut.mp4");
    let swapped = pixel(&output, 0.5, 80, 45);
    assert!(
        swapped[2] > 200 && swapped[0] < 30,
        "LUT not applied: {swapped:?}"
    );
}

#[test]
fn picture_fades_volume_keyframes_and_limiter_are_rendered() {
    let dir = tempfile::tempdir().unwrap();
    fixtures(dir.path());
    let edit = base(
        json!([{"clips":[{"start":0,"source":"red.mp4","duration":3,"fade_in":1,"audio":{"volume":0}}]}]),
        json!([{"name":"Music","clips":[{"start":0,"source":"tone.wav","duration":3,
            "volume_keyframes":[{"time":0,"volume":0},{"time":1,"volume":0},{"time":1.5,"volume":1}]}]}]),
    );
    let output = render(dir.path(), edit, "envelope.mp4");
    let early = pixel(&output, 0.1, 80, 45);
    let late = pixel(&output, 1.5, 80, 45);
    assert!(
        early[0] < 60 && late[0] > 200,
        "fade from black: {early:?} {late:?}"
    );
    assert!(
        rms(&samples(&output, 0.1, 0.8)) < 0.001,
        "keyframed silence leaked"
    );
    let full = rms(&samples(&output, 2.0, 0.8));
    // 0.5-amplitude mono sine: 0.354 RMS, then about -3 dB per channel when upmixed.
    assert!(
        (0.22..0.28).contains(&full),
        "tone at full keyframe gain: {full}"
    );

    let loud = base(
        json!([]),
        json!([{"clips":[{"start":0,"source":"tone.wav","duration":2,"volume":4}]}]),
    );
    let mut limited = loud.clone();
    limited["master"] = json!({"limiter_db": -6});
    let output = render(dir.path(), limited, "limited.mp4");
    let peak = samples(&output, 0.5, 1.0)
        .iter()
        .fold(0f32, |m, v| m.max(v.abs()));
    assert!(
        peak < 0.53,
        "limiter ceiling (0.501 plus AAC overshoot) exceeded: {peak}"
    );
    let validated =
        timeline::validate(&timeline::parse(loud.clone()).unwrap(), dir.path()).unwrap();
    let report = export::render(&validated, &dir.path().join("report.mp4"), false).unwrap();
    assert!(report["peak_dbfs"].as_f64().unwrap() > -0.1, "{report}");
    assert!(
        report["warnings"].to_string().contains("limiter_db"),
        "{report}"
    );
    let output = render(dir.path(), loud, "unlimited.mp4");
    let peak = samples(&output, 0.5, 1.0)
        .iter()
        .fold(0f32, |m, v| m.max(v.abs()));
    assert!(peak > 0.9, "no hidden limiter should apply: {peak}");
}

#[test]
fn version_one_files_are_rejected_and_migrated() {
    let dir = tempfile::tempdir().unwrap();
    fixtures(dir.path());
    let old = dir.path().join("old.json");
    let v1 = json!({"version":1,"width":160,"height":90,"fps":{"num":30,"den":1},
        "clips":[{"source":"red.mp4","duration":1,"volume":0},{"duration":0.5},{"source":"yellow.png","duration":1}],
        "audio":[{"source":"tone.wav","offset":0.5,"duration":1}]});
    std::fs::write(&old, serde_json::to_vec(&v1).unwrap()).unwrap();
    let bin = env!("CARGO_BIN_EXE_skywrite");
    let rejected = Command::new(bin)
        .args(["edit", "validate"])
        .arg(&old)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("edit migrate"));
    let new = dir.path().join("new.json");
    let migrated = Command::new(bin)
        .args(["edit", "migrate"])
        .arg(&old)
        .arg("--output")
        .arg(&new)
        .output()
        .unwrap();
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    assert_eq!(
        std::fs::read(&old).unwrap(),
        serde_json::to_vec(&v1).unwrap()
    );
    let validated = timeline::load_and_validate(&new).unwrap();
    let summary = validated.summary();
    assert_eq!(summary["duration_frames"], 75);
    assert_eq!(summary["video_clip_count"], 2);
    assert_eq!(summary["audio_clip_count"], 1);
    let clobber = Command::new(bin)
        .args(["edit", "migrate"])
        .arg(&new)
        .arg("--output")
        .arg(&new)
        .output()
        .unwrap();
    assert!(!clobber.status.success());
    let already = Command::new(bin)
        .args(["edit", "migrate"])
        .arg(&new)
        .arg("--output")
        .arg(dir.path().join("again.json"))
        .output()
        .unwrap();
    assert!(!already.status.success());
    assert!(String::from_utf8_lossy(&already.stderr).contains("already version 2"));
}

#[test]
fn premiere_xml_keeps_layers_links_speed_and_levels() {
    let dir = tempfile::tempdir().unwrap();
    fixtures(dir.path());
    let edit = json!({"version":2,"name":"Premiere & <layers>","width":160,"height":90,"fps":{"num":30000,"den":1001},
        "video":[
            {"clips":[{"start":0,"source":"red.mp4","source_in":0.5,"duration":2,"speed":1.2,"audio":{"volume":0.5,"fade_out":0.5}}]},
            {"clips":[{"start":0.5,"source":"blue.mp4","duration":1,"fade_in":0.25,"audio":{"volume":0}},{"start":1.6,"source":"yellow.png","duration":0.4,"opacity":0.5}]}],
        "audio":[{"clips":[{"start":0,"source":"tone.wav","duration":2,"volume_keyframes":[{"time":0,"volume":1},{"time":1,"volume":0.25}]}]}],
        "master":{"limiter_db":-1},
        "chapters":[{"start":0,"title":"Start"}]});
    let validated = timeline::validate(&timeline::parse(edit).unwrap(), dir.path()).unwrap();
    let output = dir.path().join("edit.xml");
    let result = export::export(&validated, "premiere", &output, false).unwrap();
    let warnings = result["warnings"].to_string();
    assert!(warnings.contains("master limiter"), "{warnings}");
    assert!(warnings.contains("not been verified"), "{warnings}");
    let check = r#"import sys,xml.etree.ElementTree as E
r=E.parse(sys.argv[1]).getroot()
assert r.tag=='xmeml' and r.attrib['version']=='5'
s=r.find('sequence')
assert s.find('name').text=='Premiere & <layers>'
assert s.find('rate/timebase').text=='30' and s.find('rate/ntsc').text=='TRUE'
assert int(s.find('duration').text)==60
v=s.findall('media/video/track'); a=s.findall('media/audio/track')
assert len(v)==2, len(v)
assert len(a)==2, len(a)  # linked sound of V1, then the tone track
red=v[0].find('clipitem')
assert red.find('start').text=='0' and red.find('end').text=='60'
assert red.find('in').text=='12' and red.find('out').text=='72', (red.find('in').text, red.find('out').text)
assert red.find(".//effect[effectid='timeremap']//parameter[parameterid='speed']/value").text.startswith("120")
links=red.findall('link'); assert len(links)==2
linked=a[0].find('clipitem')
assert linked.find(".//effect[effectid='audiolevels']") is not None
assert len(linked.findall(".//keyframe"))>=2
assert all('file://localhost/' in p.text for p in s.iter('pathurl'))
items=v[1].findall('clipitem'); assert len(items)==2
assert items[0].find(".//effect[effectid='opacity']//keyframe") is not None
assert items[1].find('stillframe').text=='TRUE'
assert len(items[0].findall('link'))==1  # muted B-roll has no linked audio
tone=a[1].find('clipitem')
assert len(tone.findall(".//effect[effectid='audiolevels']//keyframe"))==3  # points at 0 and 1s, then the end hold
assert s.find('marker/name').text=='Start'
print('ok')
"#;
    let out = process::run(
        "python3",
        &process::args(&["-c", check, output.to_str().unwrap()]),
        Duration::from_secs(30),
        1024 * 1024,
    )
    .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
}

/// Relative strength of a frequency in the decoded output (Goertzel, mono f32).
fn tone_power(path: &Path, frequency: f64) -> f64 {
    let raw = ffmpeg(&[
        "-v",
        "error",
        "-nostdin",
        "-i",
        path.to_str().unwrap(),
        "-ac",
        "1",
        "-ar",
        "48000",
        "-f",
        "f32le",
        "-",
    ]);
    let samples: Vec<f64> = raw
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64)
        .collect();
    let k = 2.0 * (2.0 * std::f64::consts::PI * frequency / 48000.0).cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for x in &samples {
        let s0 = x + k * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - k * s1 * s2) / (samples.len() as f64).powi(2)
}

#[test]
fn roles_select_stems_and_label_interchange() {
    let dir = tempfile::tempdir().unwrap();
    fixtures(dir.path());
    let edit = |video_role: Option<&str>| {
        let mut video = json!({"clips":[{"start":0,"source":"red.mp4","duration":2}]});
        if let Some(role) = video_role {
            video["role"] = json!(role);
        }
        json!({"version":2,"width":160,"height":90,"fps":{"num":30,"den":1},
            "video":[video],
            "audio":[{"role":"music","clips":[{"start":0,"source":"tone.wav","duration":2,"volume":0.5}]}]})
    };
    let path = dir.path().join("roles.json");
    std::fs::write(&path, serde_json::to_vec(&edit(Some("dialogue"))).unwrap()).unwrap();
    let bin = env!("CARGO_BIN_EXE_skywrite");
    let render = |output: &str, roles: Option<&str>| {
        let mut command = Command::new(bin);
        command
            .args(["edit", "render"])
            .arg(&path)
            .arg("--output")
            .arg(dir.path().join(output));
        if let Some(roles) = roles {
            command.args(["--roles", roles]);
        }
        command.output().unwrap()
    };
    let full = render("full.mp4", None);
    assert!(
        full.status.success(),
        "{}",
        String::from_utf8_lossy(&full.stderr)
    );
    let dialogue = render("dialogue.mp4", Some("dialogue"));
    assert!(
        dialogue.status.success(),
        "{}",
        String::from_utf8_lossy(&dialogue.stderr)
    );
    let report: Value = serde_json::from_slice(&dialogue.stdout).unwrap();
    assert_eq!(report["roles"], json!(["dialogue"]));
    let full_music = tone_power(&dir.path().join("full.mp4"), 330.0);
    let only_music = tone_power(&dir.path().join("dialogue.mp4"), 330.0);
    let only_dialogue = tone_power(&dir.path().join("dialogue.mp4"), 440.0);
    assert!(full_music > 100.0 * only_music, "{full_music} {only_music}");
    assert!(
        only_dialogue > 100.0 * only_music,
        "{only_dialogue} {only_music}"
    );
    let music = render("music.mp4", Some("music,effects"));
    assert!(music.status.success());
    assert!(
        tone_power(&dir.path().join("music.mp4"), 330.0)
            > 100.0 * tone_power(&dir.path().join("music.mp4"), 440.0)
    );

    // An audible track without a role is never guessed.
    std::fs::write(&path, serde_json::to_vec(&edit(None)).unwrap()).unwrap();
    let refused = render("refused.mp4", Some("dialogue"));
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("V1"));

    // Roles reach FCPXML audio roles and the validation summary.
    std::fs::write(&path, serde_json::to_vec(&edit(Some("dialogue"))).unwrap()).unwrap();
    let validated = timeline::load_and_validate(&path).unwrap();
    assert_eq!(validated.summary()["audio_tracks"][0]["role"], "music");
    let fcpxml = dir.path().join("roles.fcpxml");
    export::export(&validated, "fcpxml", &fcpxml, false).unwrap();
    let text = std::fs::read_to_string(&fcpxml).unwrap();
    assert!(text.contains("audioRole=\"music\""), "{text}");
    assert!(text.contains("audioRole=\"dialogue\""), "{text}");
}
