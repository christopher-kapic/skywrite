use serde_json::{Value, json};
use skywrite::{
    captions::{self, ConvertOptions},
    export, timeline,
};
use std::{fs, path::Path, process::Command};
fn run(program: &str, args: &[&str]) -> std::process::Output {
    let out = Command::new(program).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{program}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}
fn fixture(dir: &Path, mode: &str) -> std::path::PathBuf {
    fs::write(dir.join("words.json"),serde_json::to_vec(&json!({"version":1,"source":"original.mp4","duration":2,"text":"Hello world Ω","words":[{"text":"Hello","start":0.2,"end":0.6},{"text":"world","start":0.65,"end":1.0},{"text":"Ω","start":1.2,"end":1.7}]})).unwrap()).unwrap();
    fs::write(
        dir.join("cues.srt"),
        "1\n00:00:00,200 --> 00:00:01,700\nHello 'quotes' Ω &lt;safe&gt;\n\n",
    )
    .unwrap();
    let path = dir.join("edit.json");
    fs::write(&path,serde_json::to_vec_pretty(&json!({"version":1,"width":320,"height":180,"fps":{"num":30,"den":1},"clips":[{"duration":2}],"captions":[{"source":"cues.srt","language":"eng","mode":mode}],"chapters":[{"start":0,"title":"Opening 'Ω' & <safe>;#=\\"},{"start":1,"title":"Next"}]})).unwrap()).unwrap();
    path
}
fn probe(path: &Path) -> Value {
    serde_json::from_slice(
        &run(
            "ffprobe",
            &[
                "-v",
                "error",
                "-show_streams",
                "-show_chapters",
                "-of",
                "json",
                path.to_str().unwrap(),
            ],
        )
        .stdout,
    )
    .unwrap()
}
fn assert_metadata(path: &Path) {
    let p = probe(path);
    let subtitles: Vec<_> = p["streams"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["codec_type"] == "subtitle")
        .collect();
    assert_eq!(subtitles.len(), 1);
    assert_eq!(subtitles[0]["codec_name"], "mov_text");
    assert_eq!(subtitles[0]["tags"]["language"], "eng");
    let chapters = p["chapters"].as_array().unwrap();
    assert_eq!(chapters.len(), 2);
    assert_eq!(chapters[0]["tags"]["title"], "Opening 'Ω' & <safe>;#=\\");
    assert_eq!(chapters[1]["start_time"], "1.000000");
    run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-i",
            path.to_str().unwrap(),
            "-map",
            "0:v:0",
            "-map",
            "0:a?",
            "-f",
            "null",
            "-",
        ],
    );
    let decoded = run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-i",
            path.to_str().unwrap(),
            "-map",
            "0:s:0",
            "-f",
            "srt",
            "-",
        ],
    );
    let decoded = String::from_utf8(decoded.stdout).unwrap();
    assert!(decoded.contains("Hello 'quotes' Ω"), "{decoded}");
    assert_eq!(
        bright_pixels(path, "0.8"),
        0,
        "soft captions must not burn pixels"
    );
}
#[test]
fn conversion_maps_real_words_groups_and_preserves_inputs() {
    let d = tempfile::tempdir().unwrap();
    fixture(d.path(), "soft");
    let input = d.path().join("words.json");
    let before = fs::read(&input).unwrap();
    let options = ConvertOptions {
        offset: 1.0,
        scale: 2.0,
        max_words: 2,
        max_duration: 4.0,
        max_chars: 80,
    };
    for format in ["srt", "vtt"] {
        let out = d.path().join(format!("captions.{format}"));
        let result = captions::convert(&input, &out, format, &options, false).unwrap();
        assert_eq!(result["cue_count"], 2);
        let text = fs::read_to_string(&out).unwrap();
        assert!(text.contains(if format == "srt" {
            "00:00:01,400 --> 00:00:03,000"
        } else {
            "00:00:01.400 --> 00:00:03.000"
        }));
        assert!(text.contains("Hello world"));
        assert!(captions::convert(&input, &out, format, &options, false).is_err());
    }
    assert!(captions::convert(&input, &input, "srt", &options, true).is_err());
    assert_eq!(fs::read(input).unwrap(), before);
    let mut invalid = options.clone();
    invalid.offset = -2.0;
    assert!(
        captions::convert(
            &d.path().join("words.json"),
            &d.path().join("bad.srt"),
            "srt",
            &invalid,
            false
        )
        .is_err()
    );
    assert!(!d.path().join("bad.srt").exists());
    invalid = options;
    invalid.max_words = 0;
    assert!(
        captions::convert(
            &d.path().join("words.json"),
            &d.path().join("bad.srt"),
            "srt",
            &invalid,
            false
        )
        .is_err()
    );
}
#[test]
fn native_python_soft_tracks_chapters_mux_and_no_clobber() {
    let d = tempfile::tempdir().unwrap();
    let edit_file = fixture(d.path(), "soft");
    let edit = timeline::load_and_validate(&edit_file).unwrap();
    let native = d.path().join("native.mp4");
    export::render(&edit, &native, false).unwrap();
    assert_metadata(&native);
    let script = d.path().join("render.py");
    export::export(&edit, "ffmpeg", &script, false).unwrap();
    let python = d.path().join("python.mp4");
    run(
        "python3",
        &[
            script.to_str().unwrap(),
            "--output",
            python.to_str().unwrap(),
        ],
    );
    assert_metadata(&python);
    let mux = d.path().join("mux.mp4");
    captions::mux(&native, &edit, &mux, false).unwrap();
    assert_metadata(&mux);
    assert!(captions::mux(&native, &edit, &native, true).is_err());
    for protected in [&edit_file, &d.path().join("cues.srt")] {
        let before = fs::read(protected).unwrap();
        assert!(export::render(&edit, protected, true).is_err());
        assert!(export::export(&edit, "ffmpeg", protected, true).is_err());
        let out = Command::new("python3")
            .args([
                script.to_str().unwrap(),
                "--output",
                protected.to_str().unwrap(),
                "--overwrite",
            ])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert_eq!(fs::read(protected).unwrap(), before);
    }
    let before = fs::read(&native).unwrap();
    assert!(export::render(&edit, &native, false).is_err());
    assert_eq!(fs::read(native).unwrap(), before);
}
fn bright_pixels(path: &Path, time: &str) -> usize {
    let out = run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-ss",
            time,
            "-i",
            path.to_str().unwrap(),
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "pipe:1",
        ],
    );
    out.stdout
        .chunks_exact(3)
        .filter(|p| p[0] > 100 && p[1] > 100 && p[2] > 100)
        .count()
}
#[test]
fn explicit_burn_changes_actual_pixels_in_native_and_python() {
    let d = tempfile::tempdir().unwrap();
    let file = fixture(d.path(), "burn");
    let edit = timeline::load_and_validate(&file).unwrap();
    let native = d.path().join("native.mp4");
    export::render(&edit, &native, false).unwrap();
    let script = d.path().join("render.py");
    export::export(&edit, "ffmpeg", &script, false).unwrap();
    let python = d.path().join("python.mp4");
    run(
        "python3",
        &[
            script.to_str().unwrap(),
            "--output",
            python.to_str().unwrap(),
        ],
    );
    for path in [&native, &python] {
        assert!(bright_pixels(path, "0.8") > 20);
        assert_eq!(bright_pixels(path, "0.0"), 0);
        assert!(
            !probe(path)["streams"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["codec_type"] == "subtitle")
        );
    }
}
#[test]
fn legacy_defaults_bounds_and_caption_dependency_protection() {
    let d = tempfile::tempdir().unwrap();
    let file = fixture(d.path(), "soft");
    let mut spec: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    spec.as_object_mut().unwrap().remove("captions");
    spec.as_object_mut().unwrap().remove("chapters");
    let old: timeline::Edit = serde_json::from_value(spec.clone()).unwrap();
    assert!(timeline::validate(&old, d.path()).is_ok());
    for chapters in [
        json!([{"start":1,"title":"One"},{"start":0.5,"title":"Two"}]),
        json!([{"start":2,"title":"Outside"}]),
        json!([{"start":0,"title":""}]),
    ] {
        spec["chapters"] = chapters;
        let edit: timeline::Edit = serde_json::from_value(spec.clone()).unwrap();
        assert!(timeline::validate(&edit, d.path()).is_err());
    }
    spec.as_object_mut().unwrap().remove("chapters");
    spec["captions"] = json!([{"source":"words.json"}]);
    fs::write(d.path().join("original.mp4"), b"original protected bytes").unwrap();
    let edit: timeline::Edit = serde_json::from_value(spec).unwrap();
    let valid = timeline::validate(&edit, d.path()).unwrap();
    assert!(export::export(&valid, "ffmpeg", &d.path().join("original.mp4"), true).is_err());
    fs::write(
        d.path().join("cues.srt"),
        "1\n00:00:00,000 --> 00:00:03,000\nOutside\n\n",
    )
    .unwrap();
    assert!(timeline::load_and_validate(&file).is_err());
}
#[test]
fn optional_projects_keep_inert_sidecars_and_fcpxml_metadata() {
    let d = tempfile::tempdir().unwrap();
    let file = fixture(d.path(), "soft");
    let edit = timeline::load_and_validate(&file).unwrap();
    for target in ["hyperframes", "remotion"] {
        let dir = d.path().join(target);
        let result = export::export(&edit, target, &dir, false).unwrap();
        assert!(
            result["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w.as_str().unwrap().contains("sidecars"))
        );
        assert!(
            fs::read_to_string(dir.join("captions-0.srt"))
                .unwrap()
                .contains("&lt;safe&gt;")
        );
        let data: Value =
            serde_json::from_slice(&fs::read(dir.join("captions-chapters.json")).unwrap()).unwrap();
        assert_eq!(
            data["captions"][0]["cues"][0]["text"],
            "Hello 'quotes' Ω <safe>"
        );
        let code = fs::read_to_string(dir.join(if target == "hyperframes" {
            "index.html"
        } else {
            "index.jsx"
        }))
        .unwrap();
        assert!(!code.contains("Hello 'quotes'"));
        assert!(!code.contains("Opening 'Ω'"));
        assert!(!code.contains("<safe>"));
    }
    let xml = d.path().join("edit.fcpxml");
    let result = export::export(&edit, "fcpxml", &xml, false).unwrap();
    assert!(!result["warnings"].as_array().unwrap().is_empty());
    let text = fs::read_to_string(xml).unwrap();
    assert!(text.contains("<chapter-marker"));
    assert!(text.contains("<note>"));
    assert!(text.contains("&lt;safe&gt;"));
    assert!(!text.contains("<safe>"));
}

#[test]
fn srt_vtt_transcript_mapping_and_consumed_aliases_are_protected() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("quotes ' Ω");
    fs::create_dir(&dir).unwrap();
    let file = fixture(&dir, "soft");
    fs::write(
        dir.join("cues.vtt"),
        "WEBVTT\n\n1\n00:00:00.200 --> 00:00:01.700\nHello 'quotes' Ω &lt;safe&gt;\n\n",
    )
    .unwrap();
    let media = dir.join("original.mp4");
    fs::write(&media, b"preserved referenced media").unwrap();
    let mut transcript: Value =
        serde_json::from_slice(&fs::read(dir.join("words.json")).unwrap()).unwrap();
    transcript["source"] = json!("../quotes ' Ω/original.mp4");
    fs::write(
        dir.join("words.json"),
        serde_json::to_vec(&transcript).unwrap(),
    )
    .unwrap();
    for source in ["cues.srt", "cues.vtt", "words.json"] {
        let mut spec: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        spec["captions"][0]["source"] = json!(source);
        spec["captions"][0]["offset"] = json!(0.2);
        spec["captions"][0]["scale"] = json!(0.5);
        fs::write(&file, serde_json::to_vec(&spec).unwrap()).unwrap();
        let edit = timeline::load_and_validate(&file).unwrap();
        let output = dir.join(format!("data-{source}"));
        export::export(&edit, "remotion", &output, false).unwrap();
        let data: Value =
            serde_json::from_slice(&fs::read(output.join("captions-chapters.json")).unwrap())
                .unwrap();
        let cues = &data["captions"][0]["cues"];
        assert!((cues[0]["start"].as_f64().unwrap() - 0.3).abs() < 1e-9);
        assert_eq!(cues.as_array().unwrap().last().unwrap()["end"], 1.05);
        let script = dir.join(format!("render-{source}.py"));
        export::export(&edit, "ffmpeg", &script, false).unwrap();
        let mut protected = vec![file.clone(), dir.join(source)];
        if source == "words.json" {
            protected.push(media.clone());
        }
        for input in protected {
            let before = fs::read(&input).unwrap();
            for target in ["ffmpeg", "fcpxml", "remotion", "hyperframes"] {
                assert!(
                    export::export(&edit, target, &input, true).is_err(),
                    "{target}: {}",
                    input.display()
                );
            }
            assert!(export::render(&edit, &input, true).is_err());
            let out = Command::new("python3")
                .args([
                    script.to_str().unwrap(),
                    "--output",
                    input.to_str().unwrap(),
                    "--overwrite",
                ])
                .output()
                .unwrap();
            assert!(!out.status.success());
            assert_eq!(fs::read(input).unwrap(), before);
        }
    }
    let cli_output = dir.join("from-cli.vtt");
    run(
        env!("CARGO_BIN_EXE_skywrite"),
        &[
            "captions",
            "convert",
            dir.join("words.json").to_str().unwrap(),
            "--output",
            cli_output.to_str().unwrap(),
            "--format",
            "vtt",
            "--max-words",
            "2",
            "--offset",
            "1",
            "--scale",
            "2",
        ],
    );
    assert!(
        fs::read_to_string(cli_output)
            .unwrap()
            .contains("00:00:01.400 --> 00:00:03.000")
    );
}

#[test]
fn failed_native_and_python_renders_preserve_previous_outputs_and_inputs() {
    let d = tempfile::tempdir().unwrap();
    let file = fixture(d.path(), "soft");
    let media = d.path().join("input.mp4");
    run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=black:s=320x180:r=30:d=2",
            "-an",
            "-c:v",
            "libx264",
            "-threads",
            "2",
            media.to_str().unwrap(),
        ],
    );
    let mut spec: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    spec["clips"][0]["source"] = json!("input.mp4");
    fs::write(&file, serde_json::to_vec(&spec).unwrap()).unwrap();
    let edit = timeline::load_and_validate(&file).unwrap();
    let script = d.path().join("render.py");
    export::export(&edit, "ffmpeg", &script, false).unwrap();
    fs::remove_file(media).unwrap();
    let output = d.path().join("previous.mp4");
    fs::write(&output, b"previous completed output").unwrap();
    let edit_before = fs::read(&file).unwrap();
    let captions_before = fs::read(d.path().join("cues.srt")).unwrap();
    assert!(export::render(&edit, &output, true).is_err());
    assert_eq!(fs::read(&output).unwrap(), b"previous completed output");
    let out = Command::new("python3")
        .args([
            script.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--overwrite",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(fs::read(&output).unwrap(), b"previous completed output");
    assert_eq!(fs::read(&file).unwrap(), edit_before);
    assert_eq!(
        fs::read(d.path().join("cues.srt")).unwrap(),
        captions_before
    );
    assert!(!fs::read_dir(d.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".skywrite-render-")
    }));
}

#[cfg(unix)]
#[test]
fn actual_python_cancellation_preserves_old_output_and_cleans_sidecars() {
    use std::time::{Duration, Instant};
    let d = tempfile::tempdir().unwrap();
    let file = fixture(d.path(), "soft");
    let mut spec: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    spec["clips"][0]["duration"] = json!(3600);
    fs::write(&file, serde_json::to_vec(&spec).unwrap()).unwrap();
    let edit = timeline::load_and_validate(&file).unwrap();
    let script = d.path().join("render.py");
    export::export(&edit, "ffmpeg", &script, false).unwrap();
    let output = d.path().join("previous.mp4");
    fs::write(&output, b"previous completed output").unwrap();
    let temporary = d.path().join("temporary");
    fs::create_dir(&temporary).unwrap();
    let mut child = Command::new("python3")
        .args([
            script.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--overwrite",
        ])
        .env("TMPDIR", &temporary)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let started = loop {
        let started = fs::read_dir(d.path()).unwrap().any(|e| {
            let e = e.unwrap();
            e.file_name()
                .to_string_lossy()
                .starts_with(".skywrite-render-")
                && e.metadata().unwrap().len() > 0
        });
        if started || Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
            break started;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Signal only this owned Python process; its existing handler owns cleanup.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let status = child.wait().unwrap();
    assert!(
        started,
        "actual FFmpeg must write its private output before cancellation"
    );
    assert!(!status.success());
    assert_eq!(fs::read(output).unwrap(), b"previous completed output");
    assert_eq!(fs::read_dir(temporary).unwrap().count(), 0);
    assert!(!fs::read_dir(d.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".skywrite-render-")
    }));
}

// Read the actual MP4 mov_text sample rather than decoding to SRT, whose
// serializer may escape markup or interpret braces a second time.
fn literal_soft_text(path: &Path) -> String {
    let raw = run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-i",
            path.to_str().unwrap(),
            "-map",
            "0:s:0",
            "-c:s",
            "copy",
            "-f",
            "data",
            "-",
        ],
    )
    .stdout;
    let mut offset = 0;
    while offset + 2 <= raw.len() {
        let len = u16::from_be_bytes([raw[offset], raw[offset + 1]]) as usize;
        offset += 2;
        if len != 0 {
            return String::from_utf8(raw[offset..offset + len].to_vec()).unwrap();
        }
    }
    panic!("no nonempty subtitle sample");
}

#[test]
fn literal_caption_text_is_exact_or_rejected_before_publication() {
    let d = tempfile::tempdir().unwrap();
    let file = fixture(d.path(), "soft");
    let text = "<b>literal</b> Ω 'single' \"double\" & end\nsecond line";
    let cues = [captions::Cue {
        start: 0.2,
        end: 1.7,
        text: text.into(),
    }];
    fs::write(
        d.path().join("cues.srt"),
        captions::encode(&cues, "srt").unwrap(),
    )
    .unwrap();
    let edit = timeline::load_and_validate(&file).unwrap();
    let native = d.path().join("native.mp4");
    export::render(&edit, &native, false).unwrap();
    let script = d.path().join("render.py");
    export::export(&edit, "ffmpeg", &script, false).unwrap();
    let python = d.path().join("python.mp4");
    run(
        "python3",
        &[
            script.to_str().unwrap(),
            "--output",
            python.to_str().unwrap(),
        ],
    );
    let mux = d.path().join("mux.mp4");
    captions::mux(&native, &edit, &mux, false).unwrap();
    for output in [&native, &python, &mux] {
        assert_eq!(literal_soft_text(output), text);
        assert_eq!(bright_pixels(output, "0.8"), 0);
    }
    // These are benign plain caption strings, including a conventional
    // alignment-looking literal; none may be silently treated as formatting.
    for rejected in [
        r"a\Nb",
        r"a\nb",
        r"a\hb",
        r"C:\clips",
        "{ordinary}",
        r"{\an8} text",
        "}",
    ] {
        let bad = [captions::Cue {
            start: 0.2,
            end: 1.7,
            text: rejected.into(),
        }];
        for format in ["srt", "vtt"] {
            assert!(
                captions::encode(&bad, format)
                    .unwrap_err()
                    .to_string()
                    .contains("braces or backslashes")
            );
        }
        // Raw app input must fail even if it did not pass through encode().
        fs::write(
            d.path().join("cues.srt"),
            format!("1\n00:00:00,200 --> 00:00:01,700\n{rejected}\n\n"),
        )
        .unwrap();
        assert!(timeline::load_and_validate(&file).is_err());
    }
    let mut transcript: Value =
        serde_json::from_slice(&fs::read(d.path().join("words.json")).unwrap()).unwrap();
    transcript["words"][0]["text"] = json!(r"a\Nb");
    fs::write(
        d.path().join("words.json"),
        serde_json::to_vec(&transcript).unwrap(),
    )
    .unwrap();
    for format in ["srt", "vtt"] {
        let output = d.path().join(format!("rejected.{format}"));
        assert!(
            captions::convert(
                &d.path().join("words.json"),
                &output,
                format,
                &ConvertOptions::default(),
                false
            )
            .is_err()
        );
        assert!(!output.exists());
    }
}

#[test]
fn literal_burn_keeps_two_lines_at_bottom_with_python_parity() {
    let d = tempfile::tempdir().unwrap();
    let file = fixture(d.path(), "burn");
    let cues = [captions::Cue {
        start: 0.2,
        end: 1.7,
        text: "<b>literal</b> Ω\nsecond line".into(),
    }];
    fs::write(
        d.path().join("cues.srt"),
        captions::encode(&cues, "srt").unwrap(),
    )
    .unwrap();
    let edit = timeline::load_and_validate(&file).unwrap();
    let native = d.path().join("native.mp4");
    export::render(&edit, &native, false).unwrap();
    let script = d.path().join("render.py");
    export::export(&edit, "ffmpeg", &script, false).unwrap();
    let python = d.path().join("python.mp4");
    run(
        "python3",
        &[
            script.to_str().unwrap(),
            "--output",
            python.to_str().unwrap(),
        ],
    );
    let frame = |path: &Path| {
        run(
            "ffmpeg",
            &[
                "-v",
                "error",
                "-ss",
                "0.8",
                "-i",
                path.to_str().unwrap(),
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "-",
            ],
        )
        .stdout
    };
    let pixels = frame(&native);
    assert_eq!(pixels, frame(&python));
    let rows: Vec<_> = pixels
        .chunks_exact(320 * 3)
        .enumerate()
        .filter_map(|(y, row)| {
            row.chunks_exact(3)
                .any(|p| p.iter().all(|c| *c > 100))
                .then_some(y)
        })
        .collect();
    assert!(
        rows.first().copied().unwrap() > 135,
        "caption moved from bottom: {rows:?}"
    );
    let bands = rows
        .iter()
        .enumerate()
        .filter(|(index, y)| *index == 0 || **y > rows[*index - 1] + 1)
        .count();
    assert_eq!(
        bands, 2,
        "literal text lost its explicit line break: {rows:?}"
    );
    assert_eq!(bright_pixels(&native, "0.0"), 0);
}
