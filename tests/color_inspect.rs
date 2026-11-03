//! Inspect reports decoded color fields and raw MXF header labels.
use std::process::Command;

#[test]
fn inspect_reports_mxf_color_labels_alongside_ffmpeg_decoding() {
    let dir = tempfile::tempdir().unwrap();
    let mxf = dir.path().join("tagged.mxf");
    let made = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc=d=0.2:r=25,format=yuv422p10le,setparams=color_trc=bt709:color_primaries=bt709:colorspace=bt709")
        .args(["-c:v", "prores"])
        .arg(&mxf)
        .output()
        .unwrap();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let out = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .arg("inspect")
        .arg(&mxf)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let color = &value["media"][0]["color"];
    assert_eq!(color["streams"][0]["color_transfer"], "bt709");
    let descriptor = &color["mxf_header"]["descriptors"][0];
    assert_eq!(
        descriptor["transfer_characteristic"]["ul"],
        "060e2b34.0401.0101.04010101.01020000"
    );
    assert_eq!(
        descriptor["transfer_characteristic"]["registry"],
        "SMPTE-registered label"
    );
    assert!(
        color["interpretation"]
            .as_str()
            .unwrap()
            .contains("sources[].color")
    );

    // Non-MXF video still reports FFmpeg's fields, without a header scan.
    let mp4 = dir.path().join("plain.mp4");
    assert!(
        Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=d=0.2",
                "-pix_fmt",
                "yuv420p"
            ])
            .arg(&mp4)
            .status()
            .unwrap()
            .success()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_skywrite"))
        .arg("inspect")
        .arg(&mp4)
        .output()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(value["media"][0]["color"]["mxf_header"].is_null());
    assert_eq!(
        value["media"][0]["color"]["streams"][0]["pix_fmt"],
        "yuv420p"
    );
}
