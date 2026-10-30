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
    let duration = number(format, "duration")
        .or_else(|| {
            streams
                .iter()
                .filter_map(|s| s.duration)
                .max_by(f64::total_cmp)
        })
        .unwrap_or(0.0);
    if !duration.is_finite() || duration < 0.0 {
        bail!("media has an invalid duration");
    }
    Ok(MediaInfo {
        path: path.clone(),
        duration,
        start_time: number(format, "start_time").unwrap_or(0.0),
        size_bytes: std::fs::metadata(&path)?.len(),
        format_name: format["format_name"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned(),
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
            "-ss",
            &time.to_string(),
            "-protocol_whitelist",
            "file,pipe",
            "-i",
        ]);
        args.push(info.path.clone().into_os_string());
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
    if info.duration > 0.0 && start + duration > info.duration + 0.02 {
        bail!(
            "requested range exceeds media duration ({}s)",
            info.duration
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

/// Streaming content identity used to reject transcripts attached to changed media.
pub fn file_sha256(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        bail!("content hashing requires a regular local file");
    }
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
