use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command, time::Duration};

fn ffmpeg(args: &[std::ffi::OsString]) {
    skywrite::process::run("ffmpeg", args, Duration::from_secs(20), 1024 * 1024).unwrap();
}
fn image(path: &Path) -> Vec<u8> {
    let mut args = skywrite::process::args(&[
        "-v",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "color=orange:s=32x24:d=0.1",
        "-frames:v",
        "1",
    ]);
    args.push(path.as_os_str().to_owned());
    ffmpeg(&args);
    fs::read(path).unwrap()
}
fn video(path: &Path) -> Vec<u8> {
    let mut args = skywrite::process::args(&[
        "-v",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "color=orange:s=32x24:r=24:d=0.25",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=0.25",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-movflags",
        "+faststart",
        "-shortest",
    ]);
    args.push(path.as_os_str().to_owned());
    ffmpeg(&args);
    fs::read(path).unwrap()
}
fn seed(home: &Path, id: &str, task: &str, status: &str) -> std::path::PathBuf {
    let dir = home.join("jobs").join(id);
    fs::create_dir_all(&dir).unwrap();
    let job = json!({"version":1,"id":id,"task":task,"model":"test/fixture","status":status,"created_at":1,"updated_at":1,"request":{},"references":[],"remote_id":null,"generation_id":null,"usage":null,"artifact":null,"error":null});
    fs::write(dir.join("job.json"), serde_json::to_vec(&job).unwrap()).unwrap();
    dir
}
fn resume(home: &Path, id: &str, output: &Path, no_tools: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_skywrite"));
    command
        .args(["jobs", "resume", id, "--output"])
        .arg(output)
        .arg("--overwrite")
        .env("SKYWRITE_HOME", home)
        .env_remove("OPENROUTER_API_KEY")
        .env("HTTPS_PROXY", "http://127.0.0.1:1");
    if no_tools {
        command.env("PATH", "");
    }
    command.output().unwrap()
}
fn response(dir: &Path, mime: &str, bytes: &[u8]) -> Vec<u8> {
    let data = serde_json::to_vec(&json!({"data":[{"media_type":mime,"b64_json":STANDARD.encode(bytes)}],"usage":{"cost":0.25}})).unwrap();
    fs::write(dir.join("response.json"), &data).unwrap();
    data
}
fn corrupt_png() -> Vec<u8> {
    STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAAAElFTkSuQmCC")
        .unwrap()
}

#[test]
fn received_images_require_decodable_pixels_and_preserve_received_state() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let jpeg = image(&temp.path().join("good.jpg"));
    let scan = jpeg
        .windows(2)
        .position(|part| part == [0xff, 0xda])
        .unwrap();
    let webp = image(&temp.path().join("good.webp"));
    let cases = [
        ("image/png", "png", corrupt_png()),
        ("image/jpeg", "jpg", jpeg[..scan + 8].to_vec()),
        ("image/webp", "webp", webp[..30].to_vec()),
    ];
    for (index, (mime, extension, corrupt)) in cases.into_iter().enumerate() {
        assert!(
            imagesize::blob_size(&corrupt).is_ok(),
            "fixture must pass header-only dimensions"
        );
        let id = format!("{:064x}", index + 1);
        let dir = seed(&home, &id, "image-generation", "received");
        let saved_response = response(&dir, mime, &corrupt);
        let output = temp.path().join(format!("published.{extension}"));
        fs::write(&output, b"existing output").unwrap();
        for existing in [true, false] {
            if !existing {
                fs::remove_file(&output).unwrap();
            }
            let result = resume(&home, &id, &output, false);
            assert!(
                !result.status.success(),
                "undecodable provider image was accepted"
            );
            assert!(String::from_utf8_lossy(&result.stderr).contains("decode validation"));
            if existing {
                assert_eq!(fs::read(&output).unwrap(), b"existing output");
            } else {
                assert!(!output.exists());
            }
            let job: Value =
                serde_json::from_slice(&fs::read(dir.join("job.json")).unwrap()).unwrap();
            assert_eq!(job["status"], "received");
            assert!(job["artifact"].is_null());
            assert_eq!(job["usage"]["cost"], 0.25);
            assert!(job["error"].as_str().unwrap().contains("not decodable"));
            assert_eq!(fs::read(dir.join("response.json")).unwrap(), saved_response);
            assert!(!dir.join(format!("result.{extension}")).exists());
        }
    }
}

#[test]
fn healthy_images_are_verified_once_and_replay_without_decoder_or_network() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    for (index, (mime, extension)) in [
        ("image/png", "png"),
        ("image/jpeg", "jpg"),
        ("image/webp", "webp"),
    ]
    .into_iter()
    .enumerate()
    {
        let bytes = image(&temp.path().join(format!("source.{extension}")));
        let id = format!("{:064x}", index + 10);
        let dir = seed(&home, &id, "image-generation", "received");
        response(&dir, mime, &bytes);
        let output = temp.path().join(format!("good.{extension}"));
        let result = resume(&home, &id, &output, false);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let job: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(job["status"], "completed");
        assert_eq!(job["artifact"]["decode_validation_version"], 1);
        assert_eq!(fs::read(&output).unwrap(), bytes);
        fs::remove_file(&output).unwrap();
        let replay = resume(&home, &id, &output, true);
        assert!(
            replay.status.success(),
            "{}",
            String::from_utf8_lossy(&replay.stderr)
        );
        assert_eq!(fs::read(output).unwrap(), bytes);
        if extension == "jpg" {
            let png = temp.path().join("converted.png");
            let conversion = resume(&home, &id, &png, false);
            assert!(conversion.status.success());
            assert!(fs::read(png).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));
        }
    }
}

#[test]
fn legacy_completed_corrupt_image_and_video_cannot_bypass_same_format_publication() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let mut corrupt_video = video(&temp.path().join("source.mp4"));
    let mdat = corrupt_video
        .windows(4)
        .position(|part| part == b"mdat")
        .unwrap();
    let size = u32::from_be_bytes(corrupt_video[mdat - 4..mdat].try_into().unwrap()) as usize;
    corrupt_video[mdat + 4..mdat - 4 + size].fill(0);
    for (index, (task, mime, extension, bytes)) in [
        ("image-generation", "image/png", "png", corrupt_png()),
        ("video-generation", "video/mp4", "mp4", corrupt_video),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("{:064x}", index + 20);
        let dir = seed(&home, &id, task, "completed");
        let source = dir.join(format!("result.{extension}"));
        fs::write(&source, &bytes).unwrap();
        if extension == "mp4" {
            assert!(
                skywrite::media::probe(&source).unwrap().has_video(),
                "corrupt MP4 must pass metadata-only inspection"
            );
        }
        let mut job: Value =
            serde_json::from_slice(&fs::read(dir.join("job.json")).unwrap()).unwrap();
        job["artifact"] = json!({"file":format!("result.{extension}"),"media_type":mime,"bytes":bytes.len(),"sha256":skywrite::media::file_sha256(&source).unwrap()});
        fs::write(dir.join("job.json"), serde_json::to_vec(&job).unwrap()).unwrap();
        let metadata = Command::new(env!("CARGO_BIN_EXE_skywrite"))
            .args(["jobs", "status", &id])
            .env("SKYWRITE_HOME", &home)
            .env("PATH", "")
            .env_remove("OPENROUTER_API_KEY")
            .output()
            .unwrap();
        assert!(
            metadata.status.success(),
            "read-only legacy metadata must not invoke a decoder"
        );
        let recorded: Value = serde_json::from_slice(&metadata.stdout).unwrap();
        assert_eq!(recorded["artifact"]["decode_validation_version"], 0);
        let output = temp.path().join(format!("legacy.{extension}"));
        fs::write(&output, b"keep legacy output").unwrap();
        let result = resume(&home, &id, &output, false);
        assert!(
            !result.status.success(),
            "legacy undecodable cache was published"
        );
        assert!(String::from_utf8_lossy(&result.stderr).contains("decode validation"));
        assert_eq!(fs::read(output).unwrap(), b"keep legacy output");
        assert_eq!(fs::read(source).unwrap(), bytes);

        // The decode guard must precede even parsing a cleanup record or
        // constructing a cloud client. No bucket configuration or keys exist.
        job["references"] = json!([{
            "cleanup_requested":true,"cleanup_status":"pending","storage_upload":{}
        }]);
        fs::write(dir.join("job.json"), serde_json::to_vec(&job).unwrap()).unwrap();
        let cleanup = Command::new(env!("CARGO_BIN_EXE_skywrite"))
            .args(["jobs", "resume", &id])
            .env("SKYWRITE_HOME", &home)
            .env_remove("OPENROUTER_API_KEY")
            .env_remove("AWS_ACCESS_KEY_ID")
            .env_remove("AWS_SECRET_ACCESS_KEY")
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .output()
            .unwrap();
        assert!(!cleanup.status.success());
        assert!(String::from_utf8_lossy(&cleanup.stderr).contains("decode validation"));
        let retained: Value =
            serde_json::from_slice(&fs::read(dir.join("job.json")).unwrap()).unwrap();
        assert_eq!(retained["references"][0]["cleanup_status"], "pending");
    }
}
