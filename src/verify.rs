//! Optional local technical checks, without artistic suitability claims.
use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
#[derive(Debug, Args)]
pub struct VerifyArgs {
    pub input: PathBuf,
    #[arg(long)]
    pub metadata_only: bool,
    #[arg(long)]
    pub expected_duration: Option<f64>,
    #[arg(long, default_value_t = 0.05)]
    pub duration_tolerance: f64,
    #[arg(long)]
    pub expected_width: Option<u32>,
    #[arg(long)]
    pub expected_height: Option<u32>,
    #[arg(long)]
    pub expected_fps: Option<f64>,
    #[arg(long, default_value_t = 0.01)]
    pub fps_tolerance: f64,
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub overwrite: bool,
}
#[derive(Debug)]
pub struct CheckFailed {
    pub report: Value,
}
impl std::fmt::Display for CheckFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "technical verification expectations failed")
    }
}
impl std::error::Error for CheckFailed {}
pub fn execute(args: VerifyArgs) -> Result<Value> {
    execute_with_limit(args, 16 * 1024 * 1024 * 1024)
}
fn execute_with_limit(args: VerifyArgs, hash_limit: u64) -> Result<Value> {
    if !args.duration_tolerance.is_finite()
        || args.duration_tolerance < 0.0
        || !args.fps_tolerance.is_finite()
        || args.fps_tolerance < 0.0
        || args
            .expected_duration
            .is_some_and(|x| !x.is_finite() || x <= 0.0)
        || args
            .expected_fps
            .is_some_and(|x| !x.is_finite() || x <= 0.0)
        || args.expected_width == Some(0)
        || args.expected_height == Some(0)
    {
        bail!(
            "expected dimensions/rates/duration must be positive; tolerances finite and nonnegative"
        );
    }
    if let Some(path) = &args.output {
        crate::io::protect_inputs(path, std::slice::from_ref(&args.input))?;
        if path.exists() && !args.overwrite {
            bail!("output exists; use --overwrite to replace it");
        }
    }
    let info = crate::media::probe(&args.input)?;
    #[cfg(test)]
    hash_boundary_tests::after_probe();
    if info.size_bytes > hash_limit {
        bail!("verification source exceeds 16 GiB work limit");
    }
    let hash = crate::media::file_sha256_limited(&info.path, info.size_bytes.min(hash_limit))?;
    let audio = if !args.metadata_only && info.has_audio() {
        Some(crate::timing::measure_audio(&info)?)
    } else {
        None
    };
    let video = if !args.metadata_only && info.has_video() {
        Some(decode_video(&info)?)
    } else {
        None
    };
    if crate::media::file_sha256_limited(&info.path, info.size_bytes.min(hash_limit))? != hash {
        bail!("source changed during verification");
    }
    let measured_duration = match (&audio, &video) {
        (Some(a), Some(v)) => Some(
            a.presentation_end_seconds
                .max(v["presentation_end_seconds"].as_f64().unwrap()),
        ),
        (Some(a), None) => Some(a.presentation_end_seconds),
        (None, Some(v)) => v["presentation_end_seconds"].as_f64(),
        _ => None,
    };
    let mut checks = Vec::new();
    let duration = measured_duration.unwrap_or(info.duration);
    if let Some(expected) = args.expected_duration {
        checks.push(json!({"property":"duration","expected":expected,"actual":duration,"tolerance":args.duration_tolerance,"basis":if measured_duration.is_some(){"decoded_presentation"}else{"metadata_estimate"},"passed":(duration-expected).abs() <= args.duration_tolerance}));
    }
    for (name, expected, actual) in [
        (
            "width",
            args.expected_width,
            info.video().and_then(|v| v.width),
        ),
        (
            "height",
            args.expected_height,
            info.video().and_then(|v| v.height),
        ),
    ] {
        if let Some(expected) = expected {
            checks.push(json!({"property":name,"expected":expected,"actual":actual,"basis":"stream_metadata","passed":actual==Some(expected)}));
        }
    }
    if let Some(expected) = args.expected_fps {
        let actual = info
            .video()
            .and_then(|v| v.avg_frame_rate.as_deref())
            .and_then(parse_rate);
        checks.push(json!({"property":"fps","expected":expected,"actual":actual,"tolerance":args.fps_tolerance,"basis":"average_stream_metadata","passed":actual.is_some_and(|v|(v-expected).abs()<=args.fps_tolerance)}));
    }
    let passed = checks.iter().all(|c| c["passed"] == true);
    let report = json!({"version":1,"passed":passed,"source_sha256":hash,"media":info,"metadata_duration_basis":"estimate","selected_streams":"first audio and first video","full_decode_executed":!args.metadata_only,"decoded_duration_seconds":measured_duration,"audio":audio,"video":video,"checks":checks,"scope":"local technical evidence only; no creative, child-suitability, or NLE compatibility judgment"});
    if let Some(path) = &args.output {
        crate::io::write_json(path, &report, args.overwrite)?;
    }
    if !passed {
        return Err(CheckFailed { report }.into());
    }
    Ok(report)
}
fn parse_rate(rate: &str) -> Option<f64> {
    let (n, d) = rate.split_once('/')?;
    let value = n.parse::<f64>().ok()? / d.parse::<f64>().ok()?;
    (value.is_finite() && value > 0.0).then_some(value)
}
fn decode_video(info: &crate::media::MediaInfo) -> Result<Value> {
    if info.size_bytes > 16 * 1024 * 1024 * 1024 {
        bail!("verification source exceeds 16 GiB work limit");
    }
    let mut args = crate::process::args(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-xerror",
        "-threads",
        "1",
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]);
    args.push(info.path.clone().into_os_string());
    args.extend(crate::process::args(&[
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-threads",
        "1",
        "-t",
        "21601",
        "-fps_mode",
        "passthrough",
        "-f",
        "null",
        "-",
        "-progress",
        "pipe:1",
        "-stats_period",
        "60",
        "-nostats",
    ]));
    let out = crate::process::run("ffmpeg", &args, Duration::from_secs(600), 1024 * 1024)?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.lines().any(|l| l == "progress=end") {
        bail!("video decode did not complete");
    }
    let value = |key: &str| {
        stdout
            .lines()
            .filter_map(|l| l.strip_prefix(key))
            .filter_map(|v| v.trim().parse::<f64>().ok())
            .next_back()
    };
    let frames = value("frame=").context("video decode missing frame count")?;
    let end = value("out_time_us=").context("video decode missing presentation end")? / 1_000_000.0;
    if frames < 1.0 || !end.is_finite() || end <= 0.0 || end >= crate::timing::MAX_DECODE_SECONDS {
        bail!("video decode empty or exceeds six-hour work limit");
    }
    Ok(
        json!({"full_decode_executed":true,"frames":frames as u64,"presentation_end_seconds":end,"decoder":crate::timing::decoder_identity()?}),
    )
}

#[cfg(test)]
mod hash_boundary_tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Write;
    thread_local! { static AFTER_PROBE: RefCell<Option<Box<dyn FnOnce()>>> = const {RefCell::new(None)}; }
    pub(super) fn after_probe() {
        let callback = AFTER_PROBE.with(|s| s.borrow_mut().take());
        if let Some(f) = callback {
            f();
        }
    }
    #[test]
    fn metadata_only_verify_enforces_hash_cap_after_real_probe() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.wav");
        let mut args = crate::process::args(&[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=48000:cl=mono",
            "-t",
            "0.01",
        ]);
        args.push(path.clone().into_os_string());
        crate::process::run("ffmpeg", &args, Duration::from_secs(10), 65536).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < 4096);
        let changed = path.clone();
        AFTER_PROBE.with(|s| {
            *s.borrow_mut() = Some(Box::new(move || {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(changed)
                    .unwrap()
                    .write_all(&[42; 16384])
                    .unwrap();
            }))
        });
        crate::media::hash_tests::READ_BYTES.with(|n| n.set(0));
        let error = execute_with_limit(
            VerifyArgs {
                input: path,
                metadata_only: true,
                expected_duration: None,
                duration_tolerance: 0.05,
                expected_width: None,
                expected_height: None,
                expected_fps: None,
                fps_tolerance: 0.01,
                output: None,
                overwrite: false,
            },
            4096,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("admitted byte limit"), "{error}");
        assert_eq!(
            crate::media::hash_tests::READ_BYTES.with(|n| n.get()),
            0,
            "re-probed metadata cap must precede any hash read"
        );
    }
}
