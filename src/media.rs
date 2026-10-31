pub use crate::timing::{
    AudioTiming, exact_audio_timing, probe_for_timing, resolve_audio_only_timing,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub duration: f64,
    pub start_time: f64,
    pub size_bytes: u64,
    pub format_name: String,
    pub streams: Vec<MediaStream>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaStream {
    pub index: u32,
    pub codec_type: String,
    pub codec_name: Option<String>,
    pub duration: Option<f64>,
    pub start_time: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub avg_frame_rate: Option<String>,
    pub r_frame_rate: Option<String>,
    pub time_base: Option<String>,
}
impl MediaInfo {
    pub fn has_video(&self) -> bool {
        self.streams.iter().any(|s| s.codec_type == "video")
    }
    pub fn has_audio(&self) -> bool {
        self.streams.iter().any(|s| s.codec_type == "audio")
    }
    pub fn video(&self) -> Option<&MediaStream> {
        self.streams.iter().find(|s| s.codec_type == "video")
    }
    pub fn audio(&self) -> Option<&MediaStream> {
        self.streams.iter().find(|s| s.codec_type == "audio")
    }
}
/// Accept regular local files; canonical absolute paths cannot become flags or URLs.
pub fn canonical_file(path: &Path) -> Result<PathBuf> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("reading media {}", path.display()))?;
    if !meta.is_file() {
        bail!(
            "media input must be a regular local file: {}",
            path.display()
        );
    }
    path.canonicalize()
        .with_context(|| format!("resolving {}", path.display()))
}
pub fn probe(path: &Path) -> Result<MediaInfo> {
    let path = canonical_file(path)?;
    let mut args = crate::process::args(&[
        "-v",
        "error",
        "-protocol_whitelist",
        "file,pipe",
        "-show_format",
        "-show_streams",
        "-of",
        "json",
    ]);
    args.push(path.clone().into_os_string());
    let out = crate::process::run("ffprobe", &args, Duration::from_secs(30), 8 * 1024 * 1024)?;
    let raw: Value =
        serde_json::from_slice(&out.stdout).context("ffprobe returned invalid JSON")?;
    let format = &raw["format"];
    let streams = raw["streams"]
        .as_array()
        .context("ffprobe returned no streams")?
        .iter()
        .map(|s| MediaStream {
            index: s["index"].as_u64().unwrap_or(0) as u32,
            codec_type: s["codec_type"].as_str().unwrap_or("unknown").to_owned(),
            codec_name: string(s, "codec_name"),
            duration: number(s, "duration"),
            start_time: number(s, "start_time"),
            width: s["width"].as_u64().map(|v| v as u32),
            height: s["height"].as_u64().map(|v| v as u32),
            sample_rate: s["sample_rate"].as_str().and_then(|v| v.parse().ok()),
            channels: s["channels"].as_u64().map(|v| v as u32),
            avg_frame_rate: string(s, "avg_frame_rate"),
            r_frame_rate: string(s, "r_frame_rate"),
            time_base: string(s, "time_base"),
        })
        .collect::<Vec<_>>();
    if streams.is_empty() {
        bail!("media contains no streams");
    }
    let start_time = number(format, "start_time").unwrap_or(0.0);
    let format_name = format["format_name"].as_str().unwrap_or("unknown");
    // FFmpeg's Matroska/WebM demuxer exposes Segment Duration as an ending
    // presentation timestamp (the muxer includes a nonzero timestamp origin).
    // MOV/MP4 and MPEG-TS instead expose elapsed format duration. Never apply
    // the Matroska correction to those demuxers. This uses header metadata only;
    // inspection must not scan/decode the complete source.
    let matroska = format_name
        .split(',')
        .any(|name| name == "matroska" || name == "webm");
    let duration = number(format, "duration")
        .map(|duration| {
            if matroska {
                duration - start_time
            } else {
                duration
            }
        })
        .or_else(|| {
            streams
                .iter()
                .filter_map(|stream| {
                    stream.duration.map(|duration| {
                        stream.start_time.unwrap_or(start_time) + duration - start_time
                    })
                })
                .max_by(f64::total_cmp)
        })
        .unwrap_or(0.0);
    if !duration.is_finite() || duration < 0.0 {
        bail!("media has an invalid duration");
    }
    Ok(MediaInfo {
        path: path.clone(),
        duration,
        start_time,
        size_bytes: std::fs::metadata(&path)?.len(),
        format_name: format_name.to_owned(),
        streams,
    })
}
fn string(value: &Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::to_owned)
}
fn number(value: &Value, key: &str) -> Option<f64> {
    value[key]
        .as_f64()
        .or_else(|| value[key].as_str().and_then(|s| s.parse().ok()))
        .filter(|n| n.is_finite())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleFrame {
    pub time: f64,
    pub path: PathBuf,
}
/// Sample uniformly through a selected range; manifest timestamps use source seconds.
pub fn sample_frames(
    info: &MediaInfo,
    output_dir: &Path,
    count: usize,
    start: f64,
    duration: f64,
) -> Result<Vec<SampleFrame>> {
    if !info.has_video() {
        return Ok(Vec::new());
    }
    if !(1..=32).contains(&count) {
        bail!("frame count must be between 1 and 32");
    }
    validate_range(info, start, duration)?;
    std::fs::create_dir_all(output_dir)?;
    let output_dir = output_dir.canonicalize()?;
    let mut result = Vec::new();
    for i in 0..count {
        let time = if info.duration == 0.0 {
            0.0
        } else {
            start + duration * (i as f64 + 0.5) / count as f64
        };
        let dest = output_dir.join(format!("frame-{i:03}.jpg"));
        let mut args = crate::process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-n",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
        ]);
        if !info.format_name.split(',').any(|name| name == "mpegts") {
            args.splice(5..5, crate::process::args(&["-ss", &time.to_string()]));
        }
        args.push(info.path.clone().into_os_string());
        if info.format_name.split(',').any(|name| name == "mpegts") {
            args.extend(crate::process::args(&["-ss", &time.to_string()]));
        }
        args.extend(crate::process::args(&[
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-vf",
            "scale=768:768:force_original_aspect_ratio=decrease",
            "-q:v",
            "4",
        ]));
        args.push(dest.clone().into_os_string());
        crate::process::run("ffmpeg", &args, Duration::from_secs(60), 1024 * 1024)?;
        if !dest.is_file() {
            bail!("no frame decoded at {time}s");
        }
        result.push(SampleFrame { time, path: dest });
    }
    Ok(result)
}
pub fn validate_range(info: &MediaInfo, start: f64, duration: f64) -> Result<()> {
    if !start.is_finite() || start < 0.0 || !duration.is_finite() || duration <= 0.0 {
        bail!("start must be finite/nonnegative and duration finite/positive");
    }
    let source_duration = if info.has_audio() && !info.has_video() {
        exact_audio_timing(info, None)?.presentation_end_seconds
    } else {
        info.duration
    };
    if source_duration > 0.0 && start + duration > source_duration + 0.02 {
        bail!(
            "requested range exceeds media duration ({}s)",
            source_duration
        );
    }
    Ok(())
}
pub fn extract_audio(info: &MediaInfo, output: &Path, start: f64, duration: f64) -> Result<()> {
    if !info.has_audio() {
        bail!("media has no audio stream");
    }
    validate_range(info, start, duration)?;
    let mut args = crate::process::args(&[
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-n",
        "-copyts",
        "-start_at_zero",
        "-ss",
        &start.to_string(),
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]);
    args.push(info.path.clone().into_os_string());
    args.extend(crate::process::args(&[
        "-map",
        "0:a:0",
        "-vn",
        "-af",
        &format!("asetpts=PTS-{start}/TB,aresample=16000:async=0:first_pts=0"),
        "-t",
        &duration.to_string(),
        "-ac",
        "1",
        "-ar",
        "16000",
        "-c:a",
        "pcm_s16le",
    ]));
    args.push(output.into());
    crate::process::run("ffmpeg", &args, Duration::from_secs(120), 1024 * 1024)?;
    Ok(())
}

/// Content identity for a stable regular file. Work is bounded to its opened
/// size plus one growth-detection byte, without an arbitrary large-media cap.
pub fn file_sha256(path: &Path) -> Result<String> {
    file_hash(path, None)
}
/// Hash a regular file only within a caller's explicit byte admission policy.
pub fn file_sha256_limited(path: &Path, max_bytes: u64) -> Result<String> {
    file_hash(path, Some(max_bytes))
}
fn file_hash(path: &Path, max_bytes: Option<u64>) -> Result<String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("content hashing requires a regular local file");
    }
    let size = metadata.len();
    if max_bytes.is_some_and(|max| size > max) {
        bail!("content hashing source exceeds admitted byte limit");
    }
    #[cfg(test)]
    hash_tests::after_metadata();
    let (hash, bytes) = hash_reader_bounded(&file, size)?;
    if bytes != size || file.metadata()?.len() != size {
        bail!("source changed during content hashing");
    }
    Ok(hash)
}
/// The common actual-I/O boundary, including one sentinel byte to detect growth.
/// The reader may be a file or already admitted stream; no overflow byte is hashed.
pub(crate) fn hash_reader_bounded(reader: impl std::io::Read, limit: u64) -> Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let sentinel = limit
        .checked_add(1)
        .context("content hash limit is too large")?;
    let mut reader = reader.take(sentinel);
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        #[cfg(test)]
        hash_tests::READ_BYTES.with(|bytes| bytes.set(bytes.get() + count as u64));
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > limit {
            bail!("content hashing source exceeds admitted byte limit");
        }
        hash.update(&buffer[..count]);
    }
    Ok((
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        total,
    ))
}

#[cfg(test)]
pub(crate) mod hash_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::io::Write;
    thread_local! {
        pub(crate) static READ_BYTES: Cell<u64> = const { Cell::new(0) };
        static AFTER_METADATA: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }
    pub(crate) fn after_metadata() {
        let callback = AFTER_METADATA.with(|slot| slot.borrow_mut().take());
        if let Some(callback) = callback {
            callback();
        }
    }
    #[test]
    fn stable_hash_limits_empty_exact_and_unrestricted_large_inverse() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("input");
        std::fs::write(&path, b"abc").unwrap();
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(file_sha256(&path).unwrap(), expected);
        assert_eq!(file_sha256_limited(&path, 3).unwrap(), expected);
        #[cfg(unix)]
        {
            let link = temp.path().join("allowed-link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert_eq!(
                file_sha256(&link).unwrap(),
                expected,
                "generic media symlinks retain existing semantics"
            );
        }
        READ_BYTES.with(|n| n.set(0));
        assert!(
            file_sha256_limited(&path, 2)
                .unwrap_err()
                .to_string()
                .contains("admitted byte limit")
        );
        assert_eq!(
            READ_BYTES.with(Cell::get),
            0,
            "oversize fd metadata precedes payload reads"
        );
        std::fs::write(&path, []).unwrap();
        assert_eq!(
            file_sha256_limited(&path, 0).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        std::fs::write(&path, vec![42; 1024 * 1024]).unwrap();
        assert!(file_sha256_limited(&path, 16).is_err());
        READ_BYTES.with(|n| n.set(0));
        let digest = file_sha256(&path).unwrap();
        assert_eq!(
            digest,
            crate::generation::bytes_sha256(&vec![42; 1024 * 1024])
        );
        assert_eq!(
            READ_BYTES.with(Cell::get),
            1024 * 1024,
            "default has no arbitrary small-media cap"
        );
    }
    #[test]
    fn actual_file_growth_and_shrink_after_metadata_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("growing");
        for max in [None, Some(16)] {
            std::fs::write(&path, b"abcdefgh").unwrap();
            let changed = path.clone();
            AFTER_METADATA.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(changed)
                        .unwrap()
                        .write_all(&vec![42; 2 * 1024 * 1024])
                        .unwrap();
                }))
            });
            READ_BYTES.with(|n| n.set(0));
            let error = file_hash(&path, max).unwrap_err().to_string();
            assert!(error.contains("admitted byte limit"), "{error}");
            assert_eq!(
                READ_BYTES.with(Cell::get),
                9,
                "only initial8 plus sentinel consumed"
            );
        }
        std::fs::write(&path, b"abcdefgh").unwrap();
        let changed = path.clone();
        AFTER_METADATA.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::File::options()
                    .write(true)
                    .open(changed)
                    .unwrap()
                    .set_len(3)
                    .unwrap();
            }))
        });
        assert!(
            file_sha256(&path)
                .unwrap_err()
                .to_string()
                .contains("source changed")
        );
    }
    #[cfg(unix)]
    #[test]
    fn nonregular_fifo_rejects_without_waiting_for_writer() {
        use std::os::unix::ffi::OsStrExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("fifo");
        let raw = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: owned scratch pathname, ordinary owner-only FIFO mode.
        assert_eq!(unsafe { libc::mkfifo(raw.as_ptr(), 0o600) }, 0);
        let start = std::time::Instant::now();
        assert!(
            file_sha256(&path)
                .unwrap_err()
                .to_string()
                .contains("regular local file")
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }
    #[test]
    fn reader_growth_actual_sentinel_and_impossible_limit() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"small").unwrap();
        assert_eq!(file.metadata().unwrap().len(), 5);
        file.write_all(&[42; 128]).unwrap();
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0)).unwrap();
        READ_BYTES.with(|n| n.set(0));
        assert!(
            hash_reader_bounded(file, 16)
                .unwrap_err()
                .to_string()
                .contains("admitted byte limit")
        );
        assert_eq!(READ_BYTES.with(Cell::get), 17);
        assert!(
            hash_reader_bounded(&b""[..], u64::MAX)
                .unwrap_err()
                .to_string()
                .contains("limit is too large")
        );
    }
}
