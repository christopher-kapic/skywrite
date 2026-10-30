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
fn hyperframes_html_excludes_human_metadata_and_preserves_edit_data() {
    let dir = tempfile::tempdir().unwrap();
    make_fixtures(dir.path());
    let mut specification = edit();
    specification.name = "Ordinary edit".into();
    let ordinary = timeline::validate(&specification, dir.path()).unwrap();
    let ordinary_project = dir.path().join("ordinary");
    export::export(&ordinary, "hyperframes", &ordinary_project, false).unwrap();
    let expected_html = fs::read_to_string(ordinary_project.join("index.html")).unwrap();
    assert!(expected_html.contains("<title>Skywrite composition</title>"));
    assert!(expected_html.contains("alt=\"Image\""));
    for original in [
        "Ordinary edit",
        "title.png",
        "green.mp4",
        "voice.wav",
        "<take>",
    ] {
        assert!(!expected_html.contains(original), "HTML leaked {original}");
    }

    // Harmless markup-shaped text exercises downstream rewrite-sensitive
    // characters without adding executable content to any runtime fixture.
    let renamed_image = "封面 'quoted' & <caption>.png";
    fs::rename(dir.path().join("title.png"), dir.path().join(renamed_image)).unwrap();
    specification.clips[3].source = Some(renamed_image.into());
    for (index, name) in [
        "A 'quoted' & <caption> Ω 🎬",
        "Escaped-looking &lt;caption&gt; &amp; &#34; text",
    ]
    .into_iter()
    .enumerate()
    {
        specification.name = name.into();
        let validated = timeline::validate(&specification, dir.path()).unwrap();
        let project = dir.path().join(format!("text-{index}"));
        export::export(&validated, "hyperframes", &project, false).unwrap();
        assert_eq!(
            fs::read_to_string(project.join("index.html")).unwrap(),
            expected_html,
            "human metadata must not influence executable HTML"
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(project.join("edit.validated.json")).unwrap())
                .unwrap();
        assert_eq!(saved["name"], name);
        assert!(
            saved["clips"][3]["source"]
                .as_str()
                .unwrap()
                .ends_with(renamed_image)
        );
        assert_eq!(saved["duration_frames"], 45);
        for (asset, original) in [
            ("asset-0000.mp4", "speaker's $(touch HACKED) & <take>.mp4"),
            ("asset-0001.mp4", "green.mp4"),
            ("asset-0002.png", renamed_image),
            ("asset-0003.wav", "voice.wav"),
        ] {
            assert_eq!(
                fs::read(project.join("assets").join(asset)).unwrap(),
                fs::read(dir.path().join(original)).unwrap()
            );
        }
    }
    let ordinary_data: serde_json::Value =
        serde_json::from_slice(&fs::read(ordinary_project.join("edit.validated.json")).unwrap())
            .unwrap();
    assert_eq!(ordinary_data["name"], "Ordinary edit");
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

#[test]
fn delayed_video_prefix_is_counted_once_in_both_renderers() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("delayed-video.mkv");
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
            "color=red:s=160x90:r=25:d=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=3",
            "-filter:v",
            "setpts=PTS+1/TB",
            "-c:v",
            "ffv1",
            "-threads",
            "1",
            "-c:a",
            "pcm_s16le",
            input.to_str().unwrap(),
        ],
    );
    for (index, (source_in, speed, duration, fps)) in [
        (0.0, 0.5, 4.0, Rational { num: 30, den: 1 }),
        (0.0, 1.0, 2.0, Rational { num: 30, den: 1 }),
        (0.0, 2.0, 1.5, Rational { num: 30, den: 1 }),
        (
            0.5,
            1.0,
            2.0,
            Rational {
                num: 30000,
                den: 1001,
            },
        ),
        (1.5, 1.0, 1.0, Rational { num: 30, den: 1 }),
        (0.0, 1.0, 0.5, Rational { num: 30, den: 1 }),
        (0.5, 0.5, 0.5, Rational { num: 30, den: 1 }),
    ]
    .into_iter()
    .enumerate()
    {
        let mut specification = edit();
        specification.audio.clear();
        specification.fps = fps;
        specification.clips = vec![Clip {
            source: Some(input.clone()),
            source_in,
            duration,
            speed,
            volume: 1.0,
        }];
        let validated = timeline::validate(&specification, dir.path()).unwrap();
        let script = dir.path().join(format!("video-{index}.py"));
        export::export(&validated, "ffmpeg", &script, false).unwrap();
        for python in [false, true] {
            let output = dir.path().join(format!("video-{index}-{python}.mp4"));
            if python {
                command(
                    "python3",
                    &[
                        script.to_str().unwrap(),
                        "--output",
                        output.to_str().unwrap(),
                    ],
                );
            } else {
                export::render(&validated, &output, false).unwrap();
            }
            let onset = ((1.0 - source_in) / speed).max(0.0);
            if onset > 0.1 {
                let black = center(&output, &(onset.min(duration) * 0.5).to_string());
                assert!(
                    black.iter().all(|v| *v < 10),
                    "early picture source_in={source_in} speed={speed} python={python}: {black:?}"
                );
            }
            if onset + 0.2 < duration {
                let red = center(&output, &(onset + 0.15).to_string());
                assert!(
                    red[0] > 200 && red[1] < 20 && red[2] < 20,
                    "late picture source_in={source_in} speed={speed} python={python}: {red:?}"
                );
            }
            assert!(
                rms(&output, "0.05", "0.1") > 0.03,
                "audio missing from video prefix: index={index} python={python} source_in={source_in} speed={speed}"
            );
            assert!((media::probe(&output).unwrap().duration - validated.duration()).abs() < 0.06);
        }
    }
}

#[test]
fn additional_audio_preserves_content_before_delayed_video() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("audio-before-video.mkv");
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
            "color=red:s=160x90:r=30:d=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=3",
            "-vf",
            "setpts=PTS+1/TB",
            "-c:v",
            "ffv1",
            "-threads",
            "1",
            "-c:a",
            "pcm_s16le",
            source.to_str().unwrap(),
        ],
    );
    for (index, source_in) in [0.0, 0.5].into_iter().enumerate() {
        let mut specification = edit();
        specification.clips = vec![Clip {
            source: None,
            source_in: 0.0,
            duration: 0.5,
            speed: 1.0,
            volume: 1.0,
        }];
        specification.audio = vec![AudioClip {
            source: source.clone(),
            source_in,
            offset: 0.0,
            duration: 0.5,
            speed: 1.0,
            volume: 1.0,
        }];
        let validated = timeline::validate(&specification, dir.path()).unwrap();
        let script = dir.path().join(format!("audio-prefix-{index}.py"));
        export::export(&validated, "ffmpeg", &script, false).unwrap();
        for python in [false, true] {
            let output = dir
                .path()
                .join(format!("audio-prefix-{index}-{python}.mp4"));
            if python {
                command(
                    "python3",
                    &[
                        script.to_str().unwrap(),
                        "--output",
                        output.to_str().unwrap(),
                    ],
                );
            } else {
                export::render(&validated, &output, false).unwrap();
            }
            assert!(
                rms(&output, "0.05", "0.1") > 0.03,
                "additional audio prefix missing source_in={source_in} python={python}"
            );
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn python_renderer_cleans_owned_groups_on_cancel_timeout_and_failure() {
    let dir = tempfile::tempdir().unwrap();
    make_fixtures(dir.path());
    let validated = timeline::validate(&edit(), dir.path()).unwrap();
    let script = dir.path().join("render.py");
    export::export(&validated, "ffmpeg", &script, false).unwrap();
    let probe = r#"import os,sys,pathlib,subprocess,time,signal,stat,json
root=pathlib.Path(sys.argv[1]); script=root/'render.py'
helper=root/'ffmpeg'
helper.write_text('''#!/usr/bin/env python3
import os,sys,time,signal,subprocess,pathlib
signal.signal(signal.SIGTERM,signal.SIG_IGN)
marker=pathlib.Path(os.environ['MARKER'])
child=subprocess.Popen([sys.executable,'-c', 'import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); time.sleep(12)'])
marker.write_text(str(os.getpid())+' '+str(child.pid))
if os.environ['MODE']=='failure': sys.exit(7)
if os.environ['MODE']=='success': sys.exit(0)
time.sleep(12)
''');helper.chmod(0o700)
def live(pid):
 try: return pathlib.Path('/proc/'+str(pid)+'/stat').read_text().split()[2]!='Z'
 except FileNotFoundError: return False
for mode in ['term','int','timeout','failure','success']:
 marker=root/('pids-'+mode);output=root/('output-'+mode+'.mp4');output.write_bytes(b'protected')
 env=os.environ.copy();env.update(PATH=str(root)+os.pathsep+env['PATH'],MARKER=str(marker),MODE=mode)
 driver='''import runpy,sys,os,signal
m=runpy.run_path(sys.argv[1]); m['PLAN']['timeout_seconds']=0.4
original_killpg=os.killpg
def owned_killpg(pid, sig):
 try:
  os.waitid(os.P_PID,pid,os.WEXITED|os.WNOHANG|os.WNOWAIT)
 except ChildProcessError:
  # This finite fixture owns this still-live group. Clean it before reporting
  # the ordering failure so an inherited output pipe cannot hide the assertion.
  try: original_killpg(pid,signal.SIGKILL)
  except ProcessLookupError: pass
  raise AssertionError('leader reaped before final process-group signal')
 original_killpg(pid,sig)
os.killpg=owned_killpg
sys.argv=[sys.argv[1], '--output',sys.argv[2],'--overwrite']; m['main']()
'''
 p=subprocess.Popen([sys.executable,'-c',driver,str(script),str(output)],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
 pids=[]
 try:
  end=time.monotonic()+3
  while not marker.exists() and p.poll() is None and time.monotonic()<end:time.sleep(0.01)
  assert marker.exists(),mode+' child not started'
  pids=list(map(int,marker.read_text().split()))
  if mode in ['term','int']:p.send_signal(signal.SIGTERM if mode=='term' else signal.SIGINT)
  stdout,stderr=p.communicate(timeout=3)
  assert b'leader reaped before final process-group signal' not in stderr,(mode,'lost group identity',stderr)
  if mode=='success': assert p.returncode==0,(mode,p.returncode,stderr)
  else: assert p.returncode!=0,(mode,p.returncode,stderr)
  end=time.monotonic()+1
  while any(map(live,pids)) and time.monotonic()<end:time.sleep(.01)
  assert not any(map(live,pids)),(mode,'live owned processes',pids)
  assert not list(root.glob('.skywrite-render-*')),(mode,'temporary output leaked')
  if mode!='success':assert output.read_bytes()==b'protected',mode+' replaced existing output'
 finally:
  if p.poll() is None:p.kill();p.wait()
  if pids:
   try:os.killpg(pids[0],signal.SIGKILL)
   except ProcessLookupError:pass
# Real exported entrypoint protects source and refuses clobber before FFmpeg.
for output in [root/"speaker's $(touch HACKED) & <take>.mp4",root/'existing.mp4']:
 if output.name=='existing.mp4':output.write_bytes(b'kept')
 before=output.read_bytes()
 p=subprocess.run([sys.executable,str(script),'--output',str(output)],capture_output=True,timeout=3)
 assert p.returncode!=0 and output.read_bytes()==before
print('five lifecycle modes and source/no-clobber controls passed')
"#;
    command("python3", &["-c", probe, dir.path().to_str().unwrap()]);
}
