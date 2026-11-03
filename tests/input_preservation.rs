//! Actual CLI publication boundaries must preserve every consumed input.
use serde_json::{Value, json};
use std::path::Path;
use std::process::{Command, Output};

fn cli(root: &Path, arguments: &[&str], tools: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_skywrite"));
    command
        .args(arguments)
        .env("SKYWRITE_HOME", root.join("personal"))
        .env("SKYWRITE_CACHE_DIR", root.join("cache"))
        .env_remove("OPENROUTER_API_KEY");
    if !tools {
        command.env("PATH", "");
    }
    command.output().unwrap()
}
fn refused(output: Output) {
    assert!(
        !output.status.success(),
        "unexpected output: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("output would overwrite an input file"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn wav(path: &Path) {
    let output = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=16000:duration=1",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn gap(path: &Path) {
    std::fs::write(
        path,
        serde_json::to_vec(
            &json!({"version":2,"width":32,"height":32,"fps":{"num":10,"den":1},"duration":0.2}),
        )
        .unwrap(),
    )
    .unwrap();
}
fn transcript(path: &Path, source: &Path) {
    std::fs::write(path,serde_json::to_vec(&json!({"version":1,"source":source,"duration":1,"text":"hello","words":[{"text":"hello","start":0,"end":0.5}]})).unwrap()).unwrap();
}
#[cfg(unix)]
fn alias(source: &Path, target: &Path) {
    std::os::unix::fs::symlink(source, target).unwrap();
}

#[test]
fn analyze_protects_transcript_before_tools_or_paid_requests() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let media = root.join("source.wav");
    wav(&media);
    let words = root.join("words.json");
    transcript(&words, &media);
    let original = std::fs::read(&words).unwrap();
    for local in [false, true] {
        let mut arguments = vec![
            "analyze",
            media.to_str().unwrap(),
            "--transcript",
            words.to_str().unwrap(),
            "--output",
            words.to_str().unwrap(),
            "--overwrite",
        ];
        if local {
            arguments.push("--local");
        } else {
            arguments.extend(["--model", "fixture/never-called"]);
        }
        refused(cli(root, &arguments, false));
        assert_eq!(std::fs::read(&words).unwrap(), original);
    }
    #[cfg(unix)]
    {
        let link = root.join("alias.json");
        alias(&words, &link);
        refused(cli(
            root,
            &[
                "analyze",
                media.to_str().unwrap(),
                "--local",
                "--transcript",
                words.to_str().unwrap(),
                "--output",
                link.to_str().unwrap(),
                "--overwrite",
            ],
            false,
        ));
        assert_eq!(std::fs::read(&words).unwrap(), original);
        assert!(
            std::fs::symlink_metadata(link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    // A distinct existing result is explicitly replaceable, and both inputs survive.
    let output = root.join("analysis.json");
    std::fs::write(&output, b"old result").unwrap();
    let media_bytes = std::fs::read(&media).unwrap();
    let result = success(cli(
        root,
        &[
            "analyze",
            media.to_str().unwrap(),
            "--local",
            "--transcript",
            words.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--overwrite",
        ],
        true,
    ));
    assert_eq!(result["transcript"]["text"], "hello");
    assert_eq!(std::fs::read(&words).unwrap(), original);
    assert_eq!(std::fs::read(&media).unwrap(), media_bytes);
}

#[test]
fn analysis_publication_cannot_corrupt_its_cached_frame_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let media = root.join("source.mp4");
    let output = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "color=red:size=32x32:rate=10:duration=0.2",
            "-c:v",
            "libx264",
            "-threads",
            "1",
        ])
        .arg(&media)
        .output()
        .unwrap();
    assert!(output.status.success());
    let args = [
        "analyze",
        media.to_str().unwrap(),
        "--local",
        "--frames",
        "1",
    ];
    let report = success(cli(root, &args, true));
    let frame = report["frames"][0]["path"].as_str().unwrap();
    let original = std::fs::read(frame).unwrap();
    let mut invalid = args.to_vec();
    invalid.extend(["--output", frame, "--overwrite"]);
    refused(cli(root, &invalid, true));
    assert_eq!(std::fs::read(frame).unwrap(), original);
    let replay = success(cli(root, &args, true));
    assert_eq!(replay["frames_cached"], true);
}

#[test]
fn edit_json_is_protected_for_every_export_and_native_render() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    // The structured input's extension does not determine its identity.
    for suffix in ["json", "mp4", "fcpxml"] {
        let edit = root.join(format!("edit.{suffix}"));
        gap(&edit);
        let original = std::fs::read(&edit).unwrap();
        for target in ["fcpxml", "ffmpeg", "hyperframes", "remotion"] {
            refused(cli(
                root,
                &[
                    "edit",
                    "export",
                    edit.to_str().unwrap(),
                    "--to",
                    target,
                    "--output",
                    edit.to_str().unwrap(),
                    "--overwrite",
                ],
                false,
            ));
            assert_eq!(std::fs::read(&edit).unwrap(), original);
        }
        refused(cli(
            root,
            &[
                "edit",
                "render",
                edit.to_str().unwrap(),
                "--output",
                edit.to_str().unwrap(),
                "--overwrite",
            ],
            false,
        ));
        assert_eq!(std::fs::read(&edit).unwrap(), original);
        #[cfg(unix)]
        {
            let link = root.join(format!("alias.{suffix}"));
            alias(&edit, &link);
            refused(cli(
                root,
                &[
                    "edit",
                    "export",
                    edit.to_str().unwrap(),
                    "--to",
                    "fcpxml",
                    "--output",
                    link.to_str().unwrap(),
                    "--overwrite",
                ],
                false,
            ));
            refused(cli(
                root,
                &[
                    "edit",
                    "render",
                    edit.to_str().unwrap(),
                    "--output",
                    link.to_str().unwrap(),
                    "--overwrite",
                ],
                false,
            ));
            assert_eq!(std::fs::read(&edit).unwrap(), original);
        }
    }
}

#[test]
fn direct_input_guard_covers_all_json_output_commands() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let input = root.join("input.json");
    std::fs::write(&input, b"intentionally not parsed").unwrap();
    let second = root.join("second.json");
    std::fs::write(&second, b"also not parsed").unwrap();
    let p = input.to_str().unwrap();
    let q = second.to_str().unwrap();
    for arguments in [
        vec!["inspect", p, "--output", p, "--overwrite"],
        vec!["transcribe", p, "--output", p, "--overwrite"],
        vec!["analyze", p, "--local", "--output", p, "--overwrite"],
        vec![
            "sync",
            "--reference",
            p,
            "--asset",
            q,
            "--output",
            p,
            "--overwrite",
        ],
        vec![
            "sync",
            "--reference",
            p,
            "--asset",
            q,
            "--output",
            q,
            "--overwrite",
        ],
        vec![
            "edit",
            "suggest",
            "--analysis",
            p,
            "--brief",
            "test",
            "--output",
            p,
            "--overwrite",
        ],
    ] {
        refused(cli(root, &arguments, false));
    }
    assert_eq!(std::fs::read(input).unwrap(), b"intentionally not parsed");
    assert_eq!(std::fs::read(second).unwrap(), b"also not parsed");
}

#[test]
fn nested_sources_in_transcripts_and_analyses_are_preserved_before_model_calls() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let media = root.join("source.wav");
    wav(&media);
    let media_bytes = std::fs::read(&media).unwrap();
    let reference = root.join("reference.json");
    let asset = root.join("asset.json");
    transcript(&reference, Path::new("source.wav"));
    transcript(&asset, Path::new("source.wav"));
    refused(cli(
        root,
        &[
            "sync",
            "--reference",
            reference.to_str().unwrap(),
            "--asset",
            asset.to_str().unwrap(),
            "--method",
            "transcript",
            "--output",
            media.to_str().unwrap(),
            "--overwrite",
        ],
        false,
    ));
    assert_eq!(std::fs::read(&media).unwrap(), media_bytes);
    let analysis = root.join("analysis.json");
    let mut report = success(cli(
        root,
        &["analyze", media.to_str().unwrap(), "--local"],
        true,
    ));
    let frame = root.join("frame.jpg");
    std::fs::write(&frame, b"owned derived frame").unwrap();
    let extra_source = root.join("other.wav");
    std::fs::write(&extra_source, b"owned other recording").unwrap();
    report["frames"] = json!([{"time":0.5,"path":"frame.jpg"}]);
    report["transcript"] =
        json!({"version":1,"source":"other.wav","duration":1,"text":"hello","words":[]});
    std::fs::write(&analysis, serde_json::to_vec(&report).unwrap()).unwrap();
    for source in [&media, &frame, &extra_source] {
        let original = std::fs::read(source).unwrap();
        refused(cli(
            root,
            &[
                "edit",
                "suggest",
                "--analysis",
                analysis.to_str().unwrap(),
                "--brief",
                "test",
                "--model",
                "fixture/never-called",
                "--output",
                source.to_str().unwrap(),
                "--overwrite",
            ],
            false,
        ));
        assert_eq!(std::fs::read(source).unwrap(), original);
    }
}

#[test]
fn distinct_outputs_and_atomic_hardlink_publication_still_work() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let edit = root.join("edit.json");
    gap(&edit);
    let original = std::fs::read(&edit).unwrap();
    let output = root.join("result.fcpxml");
    success(cli(
        root,
        &[
            "edit",
            "export",
            edit.to_str().unwrap(),
            "--to",
            "fcpxml",
            "--output",
            output.to_str().unwrap(),
        ],
        true,
    ));
    let xml = std::fs::read(&output).unwrap();
    assert!(xml.starts_with(b"<?xml"));
    let failed = cli(
        root,
        &[
            "edit",
            "export",
            edit.to_str().unwrap(),
            "--to",
            "fcpxml",
            "--output",
            output.to_str().unwrap(),
        ],
        true,
    );
    assert!(!failed.status.success());
    assert_eq!(std::fs::read(&output).unwrap(), xml);
    std::fs::write(&output, b"replaceable result").unwrap();
    success(cli(
        root,
        &[
            "edit",
            "export",
            edit.to_str().unwrap(),
            "--to",
            "fcpxml",
            "--output",
            output.to_str().unwrap(),
            "--overwrite",
        ],
        true,
    ));
    assert_eq!(std::fs::read(&output).unwrap(), xml);
    let video = root.join("render.mp4");
    success(cli(
        root,
        &[
            "edit",
            "render",
            edit.to_str().unwrap(),
            "--output",
            video.to_str().unwrap(),
        ],
        true,
    ));
    assert_eq!(&std::fs::read(video).unwrap()[4..8], b"ftyp");
    let hardlink = root.join("distinct-hardlink.fcpxml");
    std::fs::hard_link(&edit, &hardlink).unwrap();
    success(cli(
        root,
        &[
            "edit",
            "export",
            edit.to_str().unwrap(),
            "--to",
            "fcpxml",
            "--output",
            hardlink.to_str().unwrap(),
            "--overwrite",
        ],
        true,
    ));
    assert_eq!(std::fs::read(&edit).unwrap(), original);
    assert_eq!(std::fs::read(hardlink).unwrap(), xml);
}

#[test]
fn exported_python_protects_its_own_script_and_preserves_normal_rendering() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let edit = root.join("edit.json");
    gap(&edit);
    let renderer = root.join("render.py");
    success(cli(
        root,
        &[
            "edit",
            "export",
            edit.to_str().unwrap(),
            "--to",
            "ffmpeg",
            "--output",
            renderer.to_str().unwrap(),
        ],
        true,
    ));
    let original = std::fs::read(&renderer).unwrap();
    let python = Command::new("python3")
        .args(["-c", "import sys;print(sys.executable)"])
        .output()
        .unwrap();
    let python = String::from_utf8(python.stdout).unwrap();
    let python = python.trim();
    let mut targets = vec![renderer.clone()];
    #[cfg(unix)]
    {
        let link = root.join("alias.py");
        alias(&renderer, &link);
        targets.push(link);
    }
    for target in targets {
        let output = Command::new(python)
            .arg(&renderer)
            .args(["--output", target.to_str().unwrap(), "--overwrite"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("input renderer script"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(&renderer).unwrap(), original);
    }
    let video = root.join("render.mp4");
    let output = Command::new(python)
        .arg(&renderer)
        .args(["--output", video.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(&std::fs::read(video).unwrap()[4..8], b"ftyp");
    assert_eq!(std::fs::read(renderer).unwrap(), original);
}
