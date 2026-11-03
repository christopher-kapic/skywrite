//! Bounded local audio finishing; publication always follows a full output decode.
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
const MAX_SECONDS: f64 = 3_600.0;
const MAX_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_LOOPS: usize = 128;

#[derive(Debug, Subcommand)]
pub enum AudioCommand {
    /// Normalize integrated loudness (default -18 LUFS), or sample peak only.
    Normalize(FinishArgs),
    /// Change speed while preserving pitch; requires --tempo.
    Tempo(FinishArgs),
    /// Repeat a music selection with crossfades to --duration.
    Loop(FinishArgs),
    /// Attenuate music under --sidechain narration; output contains music only.
    Duck(FinishArgs),
    /// Compose trim, tempo, normalization, looping, ducking, fades and exact padding.
    Finish(FinishArgs),
}
#[derive(Debug, Args)]
pub struct FinishArgs {
    pub input: PathBuf,
    #[arg(short, long)]
    pub output: PathBuf,
    #[arg(long)]
    pub overwrite: bool,
    /// Seconds from the first decoded audio sample, not container timestamps.
    #[arg(long, default_value_t = 0.0)]
    pub start: f64,
    /// Source selection length before tempo; defaults to remaining decoded samples.
    #[arg(long)]
    pub length: Option<f64>,
    /// Pitch-preserving speed multiplier, 0.25 through 4.
    #[arg(long)]
    pub tempo: Option<f64>,
    /// Integrated loudness target, -50 through -5 LUFS; two-pass loudnorm.
    #[arg(long, allow_hyphen_values = true)]
    pub lufs: Option<f64>,
    /// With --lufs: true-peak ceiling. Alone: sample-peak normalization (-20 to 0 dB).
    #[arg(long, allow_hyphen_values = true)]
    pub peak_db: Option<f64>,
    #[arg(long, default_value_t = 7.0)]
    pub loudness_range: f64,
    /// Exact final duration; pad silence before trimming.
    #[arg(long)]
    pub duration: Option<f64>,
    #[arg(long, requires = "duration")]
    pub r#loop: bool,
    #[arg(long, default_value_t = 0.25)]
    pub crossfade: f64,
    /// Narration controls gain; first decoded sample aligns with output second zero.
    #[arg(long)]
    pub sidechain: Option<PathBuf>,
    #[arg(long, default_value_t = 0.035)]
    pub threshold: f64,
    #[arg(long, default_value_t = 4.0)]
    pub ratio: f64,
    #[arg(long, default_value_t = 15.0)]
    pub attack_ms: f64,
    #[arg(long, default_value_t = 600.0)]
    pub release_ms: f64,
    #[arg(long, default_value_t = 0.0)]
    pub fade_in: f64,
    #[arg(long, default_value_t = 0.0)]
    pub fade_out: f64,
}
fn bounded(name: &str, value: f64, min: f64, max: f64) -> Result<()> {
    if !value.is_finite() || value < min || value > max {
        bail!("{name} must be finite and between {min} and {max}");
    }
    Ok(())
}
fn samples(seconds: f64) -> u64 {
    (seconds * RATE as f64).round() as u64
}
fn base_args() -> Vec<OsString> {
    crate::process::args(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-xerror",
        "-y",
        "-threads",
        "1",
    ])
}
fn input(args: &mut Vec<OsString>, path: &Path) {
    args.extend(crate::process::args(&[
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]));
    args.push(path.as_os_str().to_owned());
}
fn wav_output(args: &mut Vec<OsString>, path: &Path) {
    args.extend(crate::process::args(&[
        "-vn",
        "-sn",
        "-dn",
        "-threads",
        "1",
        "-ar",
        "48000",
        "-ac",
        "2",
        "-c:a",
        "pcm_s16le",
        "-f",
        "wav",
        "-fs",
        "1073741824",
    ]));
    args.push(path.as_os_str().to_owned());
}
fn run(args: &[OsString], deadline: Instant) -> Result<std::process::Output> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .context("audio finishing deadline exceeded")?;
    crate::process::run(
        "ffmpeg",
        args,
        left.min(Duration::from_secs(600)),
        1024 * 1024,
    )
}
fn filter_file(source: &Path, output: &Path, filter: &str, deadline: Instant) -> Result<()> {
    let mut args = base_args();
    input(&mut args, source);
    args.extend(crate::process::args(&[
        "-map",
        "0:a:0",
        "-filter_threads",
        "1",
        "-af",
        filter,
    ]));
    wav_output(&mut args, output);
    run(&args, deadline)?;
    Ok(())
}
fn checked_timing(path: &Path) -> Result<crate::media::AudioTiming> {
    let info = crate::media::probe(path)?;
    if info.size_bytes > MAX_BYTES {
        bail!("audio finishing input/output exceeds one GiB");
    }
    let audio = info.audio().context("media has no audio stream")?;
    if audio.channels.is_none_or(|n| n == 0 || n > 8)
        || audio.sample_rate.is_none_or(|n| n == 0 || n > 192_000)
    {
        bail!("audio finishing accepts at most eight channels and 192 kHz input");
    }
    let timing = crate::media::exact_audio_timing(&info, None)?;
    if timing.decoded_duration_seconds > MAX_SECONDS {
        bail!("audio finishing input/output exceeds one hour of decoded audio");
    }
    Ok(timing)
}
fn tempo_filter(mut speed: f64) -> String {
    let mut filters = Vec::new();
    while speed < 0.5 {
        filters.push("atempo=0.5".to_owned());
        speed /= 0.5;
    }
    while speed > 2.0 {
        filters.push("atempo=2".to_owned());
        speed /= 2.0;
    }
    filters.push(format!("atempo={speed:.12}"));
    filters.join(",")
}
fn loudness_stats(
    path: &Path,
    target: f64,
    peak: f64,
    range: f64,
    deadline: Instant,
) -> Result<Value> {
    let mut args = base_args();
    args.extend(crate::process::args(&["-loglevel", "info"]));
    input(&mut args, path);
    args.extend(crate::process::args(&[
        "-map",
        "0:a:0",
        "-filter_threads",
        "1",
        "-af",
        &format!("loudnorm=I={target}:TP={peak}:LRA={range}:print_format=json"),
        "-f",
        "null",
        "-",
    ]));
    let out = run(&args, deadline)?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let start = stderr
        .rfind('{')
        .context("loudnorm did not report measurements")?;
    let end = stderr
        .rfind('}')
        .context("loudnorm measurement was incomplete")?;
    serde_json::from_str(&stderr[start..=end]).context("invalid loudnorm measurements")
}
fn stat(stats: &Value, key: &str) -> Option<f64> {
    stats[key]
        .as_str()?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}
pub fn execute(command: AudioCommand) -> Result<Value> {
    let (mut options, operation) = match command {
        AudioCommand::Normalize(mut args) => {
            if args.lufs.is_none() && args.peak_db.is_none() {
                args.lufs = Some(-18.0);
            }
            (args, "normalize")
        }
        AudioCommand::Tempo(args) => {
            if args.tempo.is_none() {
                bail!("audio tempo requires --tempo");
            }
            (args, "tempo")
        }
        AudioCommand::Loop(mut args) => {
            if args.duration.is_none() {
                bail!("audio loop requires --duration");
            }
            args.r#loop = true;
            (args, "loop")
        }
        AudioCommand::Duck(args) => {
            if args.sidechain.is_none() {
                bail!("audio duck requires --sidechain");
            }
            (args, "duck")
        }
        AudioCommand::Finish(args) => (args, "finish"),
    };
    bounded("start", options.start, 0.0, MAX_SECONDS)?;
    for (name, value) in [("length", options.length), ("duration", options.duration)] {
        if let Some(v) = value {
            bounded(name, v, 1.0 / RATE as f64, MAX_SECONDS)?;
        }
    }
    bounded("tempo", options.tempo.unwrap_or(1.0), 0.25, 4.0)?;
    if let Some(v) = options.lufs {
        bounded("lufs", v, -50.0, -5.0)?;
    }
    if let Some(v) = options.peak_db {
        bounded("peak-db", v, -20.0, 0.0)?;
    }
    bounded("loudness-range", options.loudness_range, 1.0, 20.0)?;
    bounded("crossfade", options.crossfade, 0.0, 30.0)?;
    bounded("threshold", options.threshold, 0.000976, 1.0)?;
    bounded("ratio", options.ratio, 1.0, 20.0)?;
    bounded("attack-ms", options.attack_ms, 0.01, 2000.0)?;
    bounded("release-ms", options.release_ms, 0.01, 9000.0)?;
    bounded("fade-in", options.fade_in, 0.0, MAX_SECONDS)?;
    bounded("fade-out", options.fade_out, 0.0, MAX_SECONDS)?;
    if options.r#loop && options.duration.is_none() {
        bail!("looping requires --duration");
    }
    let extension = options
        .output
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "wav" | "mp3") {
        bail!("audio output requires .wav or .mp3");
    }
    let source = crate::media::canonical_file(&options.input)?;
    let mut consumed = vec![source.clone()];
    if let Some(side) = &options.sidechain {
        let side = crate::media::canonical_file(side)?;
        consumed.push(side.clone());
        options.sidechain = Some(side);
    }
    for path in &consumed {
        if std::fs::metadata(path)?.len() > MAX_BYTES {
            bail!("audio finishing input exceeds one GiB");
        }
    }
    crate::io::protect_inputs(&options.output, &consumed)?;
    if std::fs::symlink_metadata(&options.output).is_ok() && !options.overwrite {
        bail!("output exists; use --overwrite to replace it");
    }
    let deadline = Instant::now() + Duration::from_secs(900);
    let source_info = crate::media::probe_for_timing(&source)?;
    let source_timing = checked_timing(&source)?;
    let side_timing = options
        .sidechain
        .as_deref()
        .map(checked_timing)
        .transpose()?;
    let start_samples = samples(options.start);
    let available_samples = samples(source_timing.decoded_duration_seconds);
    if start_samples >= available_samples {
        bail!("start is beyond the decoded audio");
    }
    let selection_samples = options
        .length
        .map(samples)
        .unwrap_or(available_samples - start_samples);
    if start_samples + selection_samples > available_samples + 1 {
        bail!("source selection exceeds decoded audio duration");
    }
    if selection_samples == 0 {
        bail!("source selection rounds to zero samples");
    }
    let prepared_estimate = selection_samples as f64 / RATE as f64 / options.tempo.unwrap_or(1.0);
    if prepared_estimate > MAX_SECONDS {
        bail!("tempo-adjusted source exceeds one hour");
    }
    let parent = options
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let workspace = tempfile::Builder::new()
        .prefix(".skywrite-audio-")
        .tempdir_in(parent)?;
    let prepared = workspace.path().join("prepared.wav");
    let mut filter = format!(
        "asetpts=N/SR/TB,aresample={RATE},atrim=start_sample={start_samples}:end_sample={},asetpts=N/SR/TB",
        start_samples + selection_samples
    );
    if let Some(speed) = options.tempo {
        filter.push(',');
        filter.push_str(&tempo_filter(speed));
    }
    filter_file(&source, &prepared, &filter, deadline)?;
    let prepared_timing = checked_timing(&prepared)?;
    let mut current = prepared;
    let mut normalization = Value::Null;
    if let Some(target) = options.lufs {
        let peak = options.peak_db.unwrap_or(-2.0);
        let stats = loudness_stats(&current, target, peak, options.loudness_range, deadline)?;
        if let (Some(i), Some(tp), Some(lra), Some(thresh), Some(offset)) = (
            stat(&stats, "input_i"),
            stat(&stats, "input_tp"),
            stat(&stats, "input_lra"),
            stat(&stats, "input_thresh"),
            stat(&stats, "target_offset"),
        ) {
            let normalized = workspace.path().join("normalized.wav");
            filter_file(
                &current,
                &normalized,
                &format!(
                    "loudnorm=I={target}:TP={peak}:LRA={}:measured_I={i}:measured_TP={tp}:measured_LRA={lra}:measured_thresh={thresh}:offset={offset}:linear=true,aresample={RATE}",
                    options.loudness_range
                ),
                deadline,
            )?;
            current = normalized;
            normalization = json!({"method":"two_pass_loudnorm","target_lufs":target,"true_peak_ceiling_db":peak,"input_measurements":stats});
        } else if prepared_timing.peak_dbfs.is_none() {
            normalization = json!({"method":"silent_input_unchanged","target_achievable":false});
        } else {
            bail!("loudnorm returned nonfinite measurements for nonsilent input");
        }
    } else if let Some(target) = options.peak_db {
        if let Some(peak) = prepared_timing.peak_dbfs {
            let normalized = workspace.path().join("normalized.wav");
            filter_file(
                &current,
                &normalized,
                &format!("volume={}dB", target - peak),
                deadline,
            )?;
            current = normalized;
            normalization =
                json!({"method":"sample_peak","target_dbfs":target,"input_peak_dbfs":peak});
        } else {
            normalization = json!({"method":"silent_input_unchanged","target_achievable":false});
        }
    }
    let content_timing = checked_timing(&current)?;
    let target_samples = options
        .duration
        .map(samples)
        .unwrap_or(samples(content_timing.decoded_duration_seconds));
    if target_samples == 0 {
        bail!("output duration rounds to zero samples");
    }
    let final_duration = target_samples as f64 / RATE as f64;
    if options.fade_in > final_duration || options.fade_out > final_duration {
        bail!("fades cannot exceed the final duration");
    }
    let mut repetitions = 1;
    let crossfade_samples = samples(options.crossfade);
    if options.r#loop && target_samples > content_timing.samples {
        if crossfade_samples * 2 >= content_timing.samples {
            bail!("crossfade must be shorter than half the prepared loop source");
        }
        let stride = content_timing.samples - crossfade_samples;
        repetitions += (target_samples - content_timing.samples).div_ceil(stride) as usize;
        if repetitions > MAX_LOOPS {
            bail!("loop needs {repetitions} repetitions; maximum is {MAX_LOOPS}");
        }
        let looped = workspace.path().join("looped.wav");
        let mut args = base_args();
        for _ in 0..repetitions {
            input(&mut args, &current);
        }
        let mut graph = String::new();
        if crossfade_samples == 0 {
            for i in 0..repetitions {
                graph.push_str(&format!("[{i}:a:0]"));
            }
            graph.push_str(&format!("concat=n={repetitions}:v=0:a=1[out]"));
        } else {
            let mut last = "0:a:0".to_owned();
            for i in 1..repetitions {
                let next = format!("l{i}");
                graph.push_str(&format!(
                    "[{last}][{i}:a:0]acrossfade=ns={crossfade_samples}:c1=tri:c2=tri[{next}];"
                ));
                last = next;
            }
            graph.push_str(&format!("[{last}]anull[out]"));
        }
        args.extend(crate::process::args(&[
            "-filter_complex_threads",
            "1",
            "-filter_complex",
            &graph,
            "-map",
            "[out]",
            "-t",
            &final_duration.to_string(),
        ]));
        wav_output(&mut args, &looped);
        run(&args, deadline)?;
        current = looped;
    }
    if let Some(sidechain) = &options.sidechain {
        let ducked = workspace.path().join("ducked.wav");
        let mut args = base_args();
        input(&mut args, &current);
        input(&mut args, sidechain);
        // A short narration must never shorten the music.
        let graph = format!(
            "[1:a:0]asetpts=N/SR/TB,aresample={RATE},apad,atrim=end_sample={target_samples}[voice];[0:a:0][voice]sidechaincompress=threshold={}:ratio={}:attack={}:release={}:makeup=1[out]",
            options.threshold, options.ratio, options.attack_ms, options.release_ms
        );
        args.extend(crate::process::args(&[
            "-filter_complex_threads",
            "1",
            "-filter_complex",
            &graph,
            "-map",
            "[out]",
            "-t",
            &final_duration.to_string(),
        ]));
        wav_output(&mut args, &ducked);
        run(&args, deadline)?;
        current = ducked;
    }
    let final_path = workspace.path().join(format!("finished.{extension}"));
    // Pad before trim: tempo/filter tails cannot leave 179.2s for a 180s request.
    let mut final_filter =
        format!("aresample={RATE},apad,atrim=end_sample={target_samples},asetpts=N/SR/TB");
    if options.fade_in > 0.0 {
        final_filter.push_str(&format!(",afade=t=in:ss=0:ns={}", samples(options.fade_in)));
    }
    if options.fade_out > 0.0 {
        let n = samples(options.fade_out);
        final_filter.push_str(&format!(",afade=t=out:ss={}:ns={n}", target_samples - n));
    }
    let mut args = base_args();
    input(&mut args, &current);
    args.extend(crate::process::args(&[
        "-map",
        "0:a:0",
        "-vn",
        "-sn",
        "-dn",
        "-filter_threads",
        "1",
        "-af",
        &final_filter,
        "-threads",
        "1",
        "-ar",
        "48000",
        "-ac",
        "2",
        "-fs",
        "1073741824",
    ]));
    if extension == "wav" {
        args.extend(crate::process::args(&["-c:a", "pcm_s16le", "-f", "wav"]));
    } else {
        args.extend(crate::process::args(&[
            "-c:a",
            "libmp3lame",
            "-b:a",
            "192k",
            "-write_xing",
            "1",
            "-f",
            "mp3",
        ]));
    }
    args.push(final_path.as_os_str().to_owned());
    run(&args, deadline)?;
    // Publication requires a fresh decode, even if these exact bytes were cached before.
    let output_timing = crate::timing::measure_audio(&crate::media::probe(&final_path)?)?;
    if output_timing.samples != target_samples || output_timing.sample_rate != RATE {
        bail!(
            "decoded output duration mismatch: requested {target_samples} samples, decoded {} at {} Hz",
            output_timing.samples,
            output_timing.sample_rate
        );
    }
    let output_info = crate::media::probe(&final_path)?;
    let codec = output_info.audio().and_then(|a| a.codec_name.as_deref());
    if (extension == "wav" && codec != Some("pcm_s16le"))
        || (extension == "mp3" && codec != Some("mp3"))
    {
        bail!("encoded audio format does not match requested extension");
    }
    // Recheck every source identity before the single publication boundary.
    if !source_identity_matches(&source, &source_timing.source_sha256, MAX_BYTES)? {
        bail!("source changed during audio finishing");
    }
    if let (Some(side), Some(timing)) = (&options.sidechain, &side_timing)
        && !source_identity_matches(side, &timing.source_sha256, MAX_BYTES)?
    {
        bail!("sidechain changed during audio finishing");
    }
    crate::io::protect_inputs(&options.output, &consumed)?;
    let mut publication = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(
        &mut std::fs::File::open(&final_path)?,
        publication.as_file_mut(),
    )?;
    publication.as_file().sync_all()?;
    if options.overwrite {
        publication.persist(&options.output).map_err(|e| e.error)?;
    } else {
        crate::io::persist_new(publication, &options.output)?;
    }
    Ok(
        json!({"version":1,"operation":operation,"output":options.output,"format":extension,"codec":codec,"sample_rate":RATE,"channels":2,"source":source,"source_sha256":source_timing.source_sha256,"source_timing":source_timing,"source_selection":{"time_basis":"seconds from first decoded audio sample; output rebased to zero","container_start_seconds":source_info.start_time,"audio_stream_start_seconds":source_info.audio().and_then(|s|s.start_time),"requested_start_seconds":options.start,"start_samples_48000":start_samples,"length_samples_48000":selection_samples,"tempo":options.tempo.unwrap_or(1.0)},"normalization":normalization,"loop_repetitions":repetitions,"crossfade_samples":crossfade_samples,"sidechain":options.sidechain,"sidechain_timing":side_timing,"requested_duration_seconds":options.duration,"rounded_target_samples":target_samples,"actual_duration_seconds":output_timing.decoded_duration_seconds,"rounding":"nearest sample at 48000 Hz; exact decoded sample count checked","output_timing":output_timing,"full_output_decode_executed":true,"note":"Normalization precedes loops, ducking and fades; these can change final loudness. Duck output contains the music only."}),
    )
}

fn source_identity_matches(path: &std::path::Path, expected: &str, limit: u64) -> Result<bool> {
    Ok(crate::media::file_sha256_limited(path, limit)? == expected)
}
#[cfg(test)]
mod hash_boundary_tests {
    use super::*;
    #[test]
    fn finishing_recheck_enforces_source_and_sidechain_hash_cap() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["source", "sidechain"] {
            let path = temp.path().join(name);
            std::fs::write(&path, b"abcdefgh").unwrap();
            let hash = crate::media::file_sha256(&path).unwrap();
            assert!(source_identity_matches(&path, &hash, 16).unwrap());
            // An original admission saw8bytes; source grew before final recheck.
            std::fs::write(&path, vec![42; 2 * 1024 * 1024]).unwrap();
            crate::media::hash_tests::READ_BYTES.with(|n| n.set(0));
            assert!(
                source_identity_matches(&path, &hash, 16)
                    .unwrap_err()
                    .to_string()
                    .contains("admitted byte limit")
            );
            assert_eq!(crate::media::hash_tests::READ_BYTES.with(|n| n.get()), 0);
        }
    }
}
