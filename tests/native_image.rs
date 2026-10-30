use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Output},
};

fn render(file: &Path, output: &Path, args: &[&str]) -> Output {
    // This boundary must work with no Takumi, FFmpeg, Node, or shell executable on PATH.
    let mut command = Command::new(env!("CARGO_BIN_EXE_skywrite"));
    command
        .env("PATH", "")
        .args(["image", "render", "--file"])
        .arg(file)
        .arg("-o")
        .arg(output);
    if !args.contains(&"--width") {
        command.args(["--width", "320"]);
    }
    if !args.contains(&"--height") {
        command.args(["--height", "160"]);
    }
    command.args(args).output().unwrap()
}
fn pixels(path: &Path) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-threads", "1", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout.len(), 320 * 160 * 4);
    out.stdout
}
fn success(out: Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
fn failure(out: Output, expected: &str) {
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(expected),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn native_html_typography_transparency_and_quality_codecs_without_path() {
    let d = tempfile::tempdir().unwrap();
    let html = d.path().join("card.html");
    std::fs::write(&html, r#"<div style="width:320px;height:160px;display:flex;align-items:center;justify-content:center;background:transparent"><span style="font-family:Geist;font-size:44px;font-weight:700;color:#ef1744">Skywrite</span></div>"#).unwrap();
    let png = d.path().join("card.png");
    let meta = success(render(&html, &png, &[]));
    assert_eq!(meta["renderer"], "takumi");
    assert_eq!(meta["renderer_version"], "2.19.0");
    let raw = pixels(&png);
    assert_eq!(&raw[..4], &[0, 0, 0, 0]);
    let painted = raw
        .chunks_exact(4)
        .filter(|p| p[3] > 100 && p[0] > 150 && p[1] < 100)
        .count();
    assert!(
        painted > 500,
        "font must paint actual colored glyphs: {painted}"
    );
    assert!(painted < 15000, "background remains transparent");
    for format in ["webp", "jpeg"] {
        let low = d.path().join(format!("low.{format}"));
        let high = d.path().join(format!("high.{format}"));
        success(render(&html, &low, &["--quality", "10"]));
        success(render(&html, &high, &["--quality", "95"]));
        assert_ne!(std::fs::read(&low).unwrap(), std::fs::read(&high).unwrap());
        let raw = pixels(&high);
        assert!(raw.chunks_exact(4).any(|p| p[0] > 140 && p[1] < 120));
        if format == "webp" {
            assert_eq!(raw[3], 0);
        } else {
            assert!(raw.chunks_exact(4).all(|p| p[3] == 255));
        }
    }
    // Script and shell-looking text inside a script are discarded by the upstream parser.
    let scripted = d.path().join("scripted.html");
    let scripted_png = d.path().join("scripted.png");
    let original = std::fs::read_to_string(&html).unwrap();
    std::fs::write(
        &scripted,
        format!("<script>throw new Error('must never run');</script>{original}"),
    )
    .unwrap();
    success(render(&scripted, &scripted_png, &[]));
    assert_eq!(
        std::fs::read(&scripted_png).unwrap(),
        std::fs::read(&png).unwrap()
    );
    failure(render(&html, &png, &[]), "output exists");
    failure(
        render(&html, &html, &["--format", "png", "--overwrite"]),
        "overwrite an input",
    );
    success(render(&html, &png, &["--overwrite"]));
}

#[test]
fn native_json_local_raster_and_font_resources() {
    let d = tempfile::tempdir().unwrap();
    let input = d.path().join("node.json");
    let out = d.path().join("node.png");
    std::fs::write(&input, serde_json::to_vec(&json!({"type":"container","style":{"width":320,"height":160,"backgroundColor":"#1734ea","display":"flex","alignItems":"center"},"children":[{"type":"text","text":"Native JSON","style":{"color":"white","fontSize":32}}]})).unwrap()).unwrap();
    let font = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/fonts/Geist.woff2");
    success(render(&input, &out, &["--font", font.to_str().unwrap()]));
    let raw = pixels(&out);
    assert!(
        raw.chunks_exact(4)
            .filter(|p| p[2] > 200 && p[0] < 60)
            .count()
            > 10000
    );
    assert!(
        raw.chunks_exact(4)
            .any(|p| p[0] > 220 && p[1] > 220 && p[2] > 220)
    );
    let local = d.path().join("local.html");
    std::fs::write(&local, "<img src='node.png' width='320' height='160'>").unwrap();
    let copy = d.path().join("copy.png");
    success(render(&local, &copy, &[]));
    assert_eq!(pixels(&copy), raw);
    failure(render(&local, &out, &["--overwrite"]), "overwrite an input");
    assert_eq!(pixels(&out), raw);
    let ownfont = d.path().join("font.woff2");
    std::fs::copy(&font, &ownfont).unwrap();
    let original_font = std::fs::read(&ownfont).unwrap();
    failure(
        render(
            &input,
            &ownfont,
            &[
                "--font",
                ownfont.to_str().unwrap(),
                "--format",
                "png",
                "--overwrite",
            ],
        ),
        "overwrite an input",
    );
    assert_eq!(std::fs::read(&ownfont).unwrap(), original_font);
}

#[test]
fn native_rejects_bad_and_bounded_inputs_without_outputs() {
    let d = tempfile::tempdir().unwrap();
    let input = d.path().join("input.json");
    let out = d.path().join("out.png");
    for data in [
        "{",
        r#"{"type":"unknown"}"#,
        r#"{"type":"image","src":"https://example.com/a.png"}"#,
        r#"{"type":"image","src":"x.png","width":1e100}"#,
    ] {
        std::fs::write(&input, data).unwrap();
        assert!(!render(&input, &out, &[]).status.success());
        assert!(!out.exists());
    }
    std::fs::write(&input, vec![b' '; 1024 * 1024 + 1]).unwrap();
    failure(render(&input, &out, &[]), "byte limit");
    let html = d.path().join("input.html");
    std::fs::write(&html, "<div>x</div>").unwrap();
    failure(
        render(&html, &out, &["--width", "8192", "--height", "8192"]),
        "canvas pixels",
    );
    failure(render(&html, &out, &["--width", "0"]), "canvas pixels");
    failure(
        render(&html, &out, &["--quality", "101", "--format", "jpeg"]),
        "quality",
    );
    failure(render(&html, &out, &["--quality", "75"]), "applies only");
    std::fs::write(
        &html,
        format!("{}x{}", "<div>".repeat(70), "</div>".repeat(70)),
    )
    .unwrap();
    failure(render(&html, &out, &[]), "maximum depth");
    std::fs::write(&html, "<i>x</i>".repeat(5000)).unwrap();
    failure(render(&html, &out, &[]), "tag limit");
    std::fs::write(&html, "<style>div{color:red}</style><div>x</div>").unwrap();
    failure(render(&html, &out, &[]), "inline style");
    assert!(!out.exists());
    std::fs::write(
        &html,
        "<div style='background:red;width:1px;height:1px'></div>",
    )
    .unwrap();
    success(render(&html, &out, &["--width", "1", "--height", "1"]));
    assert_eq!(
        imagesize::size(&out).unwrap(),
        imagesize::ImageSize {
            width: 1,
            height: 1
        }
    );
    let before = std::fs::read(&out).unwrap();
    let bad_font = d.path().join("oversized.ttf");
    std::fs::File::create(&bad_font)
        .unwrap()
        .set_len(16 * 1024 * 1024 + 1)
        .unwrap();
    failure(
        render(
            &html,
            &out,
            &["--overwrite", "--font", bad_font.to_str().unwrap()],
        ),
        "byte limit",
    );
    assert_eq!(std::fs::read(&out).unwrap(), before);
    let many = json!({"type":"container","children":(0..4097).map(|_| json!({"type":"text","text":"x"})).collect::<Vec<_>>()});
    std::fs::write(&input, serde_json::to_vec(&many).unwrap()).unwrap();
    failure(render(&input, &out, &["--overwrite"]), "node limit");
    assert_eq!(std::fs::read(&out).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn native_rejects_non_regular_input_and_protects_symlink_targets() {
    use std::os::unix::fs::symlink;
    let d = tempfile::tempdir().unwrap();
    let input = d.path().join("input.html");
    let out = d.path().join("out.png");
    let path = std::ffi::CString::new(input.as_os_str().as_encoded_bytes()).unwrap();
    // O_NONBLOCK/regular-file validation must return without waiting for a writer.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let start = std::time::Instant::now();
    assert!(!render(&input, &out, &[]).status.success());
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    std::fs::remove_file(&input).unwrap();
    std::fs::write(&input, "<div>Hello</div>").unwrap();
    symlink(&input, &out).unwrap();
    failure(render(&input, &out, &["--overwrite"]), "overwrite an input");
    assert_eq!(std::fs::read_to_string(&input).unwrap(), "<div>Hello</div>");
    assert!(out.is_symlink());
}

#[test]
fn retired_external_backend_is_rejected_before_any_input_write() {
    let d = tempfile::tempdir().unwrap();
    let input = d.path().join("source.html");
    let original = b"<div>preserve source</div>";
    std::fs::write(&input, original).unwrap();
    let out = render(
        &input,
        &input,
        &["--backend", "takumi-cli", "--format", "png", "--overwrite"],
    );
    failure(out, "unexpected argument '--backend'");
    assert_eq!(std::fs::read(&input).unwrap(), original);
}
