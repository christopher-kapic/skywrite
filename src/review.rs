//! Review aids: exact-frame contact sheets, cut audition clips, and objective
//! edit checks. These report evidence for a human or agent to judge; they never
//! change an edit and never encode artistic preferences.
use crate::{
    export, media, process,
    timeline::{self, MediaKind, ValidatedEdit},
    transcript::{Transcript, Word},
};
use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fmt::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};

pub const MAX_SHEET_FRAMES: usize = 48;
const MAX_CUTS: usize = 200;
const MAX_MEASUREMENTS: usize = 256;

#[derive(Subcommand)]
pub enum ReviewCommand {
    /// Contact sheet of consecutive exact frames from a source range, labelled with source time.
    Sheet(SheetArgs),
    /// Short audition clips around every cut in an edit, for listening and watching.
    Cuts(CutsArgs),
    /// Objective checks: silent visible speakers, repeated or clipped words, sound cut off mid-flow, beds near dialogue level.
    Lint(LintArgs),
}

#[derive(Args)]
pub struct SheetArgs {
    pub source: PathBuf,
    /// Source seconds from the first presented sample.
    #[arg(long, default_value_t = 0.0)]
    pub start: f64,
    #[arg(long)]
    pub duration: f64,
    /// Take every Nth decoded frame (2 = every other frame).
    #[arg(long, conflicts_with = "interval")]
    pub every: Option<u32>,
    /// Take one frame per this many seconds instead of a frame count.
    #[arg(long)]
    pub interval: Option<f64>,
    #[arg(long, default_value_t = 4)]
    pub columns: u32,
    /// Width of each tile in pixels.
    #[arg(long, default_value_t = 320)]
    pub tile_width: u32,
    #[arg(short, long)]
    pub output: PathBuf,
    #[arg(long)]
    pub overwrite: bool,
}

#[derive(Args)]
pub struct CutsArgs {
    pub edit: PathBuf,
    /// Seconds of context before and after each cut.
    #[arg(long, default_value_t = 1.5)]
    pub handles: f64,
    /// A new directory for the audition clips.
    #[arg(long)]
    pub output_dir: PathBuf,
    /// Reuse an existing render of this edit instead of rendering it again.
    #[arg(long)]
    pub render: Option<PathBuf>,
}

#[derive(Args)]
pub struct LintArgs {
    pub edit: PathBuf,
    /// Word-timed transcript of a dialogue source; repeat for each source.
    #[arg(long)]
    pub transcript: Vec<PathBuf>,
    /// Skip audio level measurements (faster; disables level checks).
    #[arg(long)]
    pub no_levels: bool,
}

pub fn execute(command: ReviewCommand) -> Result<Value> {
    match command {
        ReviewCommand::Sheet(args) => sheet(&args),
        ReviewCommand::Cuts(args) => cuts(&args),
        ReviewCommand::Lint(args) => lint(&args),
    }
}

// ---------------------------------------------------------------- sheets

pub struct Extracted {
    pub path: PathBuf,
    /// Presentation time in source seconds from the first presented sample.
    pub time: f64,
}

/// Which decoded frames to keep from a source range.
pub struct FrameSelection {
    /// Source seconds from the first presented sample.
    pub start: f64,
    pub duration: f64,
    /// Keep every Nth decoded frame.
    pub every: Option<u32>,
    /// Keep the first frame at or after each interval boundary.
    pub interval: Option<f64>,
    /// Output width in pixels; height keeps the aspect ratio.
    pub width: u32,
}

/// Decode a range and keep exact frames, recording each frame's presentation time.
pub fn extract_frames(
    info: &media::MediaInfo,
    selection: &FrameSelection,
    dir: &Path,
    limit: usize,
) -> Result<Vec<Extracted>> {
    let FrameSelection {
        start,
        duration,
        every,
        interval,
        width,
    } = *selection;
    ensure!(info.has_video(), "source has no video stream");
    ensure!(
        start.is_finite() && start >= 0.0 && duration.is_finite() && duration > 0.0,
        "start must be nonnegative and duration positive"
    );
    ensure!(
        start + duration <= info.duration + 0.05,
        "range ends after the source ({:.3}s)",
        info.duration
    );
    // Select on absolute source frame numbers / time buckets (not counted from
    // the range start) so overlapping passes land on the same frames and share
    // cached results. With -copyts, `t` is the source presentation time.
    let video = info.video().context("source has no video stream")?;
    let fps = video
        .avg_frame_rate
        .as_deref()
        .and_then(parse_rate)
        .filter(|r| r.is_finite() && *r > 0.0);
    let origin = video.start_time.unwrap_or(info.start_time);
    let index = |fps: f64| format!("round((t-{origin:.9})*{fps:.9})");
    let select = match (every, interval, fps) {
        (Some(n), None, Some(fps)) => {
            ensure!((1..=10_000).contains(&n), "--every must be 1..10000");
            format!("select='eq(mod({}\\,{n})\\,0)'", index(fps))
        }
        (Some(n), None, None) => {
            ensure!((1..=10_000).contains(&n), "--every must be 1..10000");
            format!("select='not(mod(n\\,{n}))'")
        }
        (None, Some(seconds), _) => {
            ensure!(
                seconds.is_finite() && seconds > 0.0,
                "--interval must be positive"
            );
            // The first frame at or after each absolute interval boundary.
            // The epsilon keeps a boundary frame whose remainder computes as
            // S minus a rounding error from being skipped.
            let tolerance = fps.map(|fps| 1.0 / fps).unwrap_or(0.04);
            format!("select='lt(mod(t-{origin:.9}+0.000001\\,{seconds:.9})\\,{tolerance:.9})'")
        }
        (None, None, _) => "select=1".into(),
        _ => bail!("choose --every or --interval"),
    };
    // Input -ss is relative to the container start even with -copyts; -copyts
    // keeps filter `t` and printed timestamps absolute.
    let seek = start;
    let mut args = process::args(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "info",
        "-n",
        "-copyts",
        "-protocol_whitelist",
        "file,pipe",
        "-ss",
        &format!("{seek:.9}"),
        "-t",
        &format!("{duration:.9}"),
        "-i",
    ]);
    args.push(info.path.clone().into_os_string());
    args.extend(process::args(&[
        "-map",
        "0:v:0",
        "-vf",
        &format!("{select},showinfo,scale={width}:-2"),
        "-fps_mode",
        "passthrough",
        "-frames:v",
        &(limit + 1).to_string(),
        "-q:v",
        "3",
    ]));
    args.push(dir.join("f%04d.jpg").into_os_string());
    let out = process::run("ffmpeg", &args, Duration::from_secs(120), 16 * 1024 * 1024)?;
    let log = String::from_utf8_lossy(&out.stderr);
    // Exact times from the integer pts and time base; printed pts_time can be
    // rounded to a few significant digits by some FFmpeg versions.
    let time_base = log
        .lines()
        .filter(|l| l.contains("Parsed_showinfo"))
        .find_map(|l| l.split("time_base:").nth(1))
        .and_then(|rest| rest.split([',', ' ']).find(|t| !t.is_empty()))
        .and_then(|value| {
            let (n, d) = value.split_once('/')?;
            let (n, d) = (n.trim().parse::<f64>().ok()?, d.trim().parse::<f64>().ok()?);
            (n > 0.0 && d > 0.0).then_some(n / d)
        });
    let mut times = Vec::new();
    for line in log
        .lines()
        .filter(|l| l.contains("Parsed_showinfo") && l.contains("pts_time:"))
    {
        let exact = time_base.and_then(|base| {
            let pts = line.split(" pts:").nth(1)?.split_whitespace().next()?;
            Some(pts.parse::<i64>().ok()? as f64 * base)
        });
        let printed = line
            .split("pts_time:")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<f64>().ok());
        if let Some(time) = exact.or(printed) {
            times.push(((time - info.start_time) * 1e9).round() / 1e9);
        }
    }
    let mut frames = Vec::new();
    for (index, time) in times.into_iter().enumerate() {
        let path = dir.join(format!("f{:04}.jpg", index + 1));
        if path.is_file() {
            frames.push(Extracted { path, time });
        }
    }
    ensure!(
        frames.len() <= limit,
        "the range yields more than {limit} frames; narrow --duration or increase --every/--interval"
    );
    ensure!(!frames.is_empty(), "no frames were decoded in that range");
    Ok(frames)
}

fn sheet(args: &SheetArgs) -> Result<Value> {
    ensure!((1..=12).contains(&args.columns), "--columns must be 1..12");
    ensure!(
        (64..=1280).contains(&args.tile_width),
        "--tile-width must be 64..1280"
    );
    let info = media::probe_for_timing(&args.source)?;
    io_guard(
        &args.output,
        std::slice::from_ref(&info.path),
        args.overwrite,
    )?;
    let temp = tempfile::Builder::new()
        .prefix("skywrite-sheet-")
        .tempdir()?;
    let frames = extract_frames(
        &info,
        &FrameSelection {
            start: args.start,
            duration: args.duration,
            every: args.every,
            interval: args.interval,
            width: args.tile_width,
        },
        temp.path(),
        MAX_SHEET_FRAMES,
    )?;
    let video = info.video().context("source has no video stream")?;
    let (w, h) = (
        video.width.unwrap_or(16) as f64,
        video.height.unwrap_or(9) as f64,
    );
    let tile_height = ((args.tile_width as f64 * h / w).round() as u32).max(16);
    let label = 28u32;
    let columns = args.columns.min(frames.len() as u32);
    let rows = (frames.len() as u32).div_ceil(columns);
    let width = columns * args.tile_width;
    let height = rows * (tile_height + label);
    ensure!(
        u64::from(width) * u64::from(height) <= 16 * 1024 * 1024,
        "contact sheet would exceed 16 megapixels; use fewer frames or smaller tiles"
    );
    let fps = video
        .avg_frame_rate
        .as_deref()
        .and_then(parse_rate)
        .filter(|r| *r > 0.0);
    let video_start = video.start_time.unwrap_or(info.start_time) - info.start_time;
    let mut html = format!(
        "<div style=\"display:flex;flex-wrap:wrap;width:{width}px;height:{height}px;background-color:#111111\">"
    );
    let mut tiles = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        let frame_number = fps.map(|fps| ((frame.time - video_start) * fps).round() as i64);
        let caption = match frame_number {
            Some(n) => format!("#{index}  {:.3}s  f{n}", frame.time),
            None => format!("#{index}  {:.3}s", frame.time),
        };
        write!(
            html,
            "<div style=\"display:flex;flex-direction:column;width:{tw}px;height:{th}px\"><img src=\"{name}\" style=\"width:{tw}px;height:{ih}px;object-fit:contain\"/><div style=\"height:{label}px;color:#ffffff;font-size:16px;padding:4px 6px\">{caption}</div></div>",
            tw = args.tile_width,
            th = tile_height + label,
            ih = tile_height,
            name = frame
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .context("frame name")?,
        )?;
        tiles.push(json!({"index":index,"source_time":frame.time,"frame":frame_number}));
    }
    html.push_str("</div>");
    let template = temp.path().join("sheet.html");
    std::fs::write(&template, html)?;
    let format = match args.output.extension().and_then(|e| e.to_str()) {
        Some("png") => "png",
        Some("webp") => "webp",
        _ => "jpeg",
    };
    crate::image::render(&crate::image::RenderImage {
        file: &template,
        output: &args.output,
        width,
        height,
        format: Some(format),
        quality: (format != "png").then_some(85),
        fonts: &[],
        google_fonts: &[],
        cache_dir: None,
        offline: true,
        overwrite: args.overwrite,
    })?;
    Ok(json!({
        "output": args.output.canonicalize()?,
        "source": info.path,
        "columns": columns,
        "rows": rows,
        "tile_width": args.tile_width,
        "tiles": tiles,
        "frame_numbers": if fps.is_some() {"estimated from the average frame rate; exact for constant-frame-rate sources"} else {"unknown frame rate"},
        "note": "Tiles are consecutive decoded frames with their presentation times in source seconds (from the first presented sample), read left to right. Use them to judge expressions, motion and timing around a candidate cut; describe frames with your own image tools.",
    }))
}

fn parse_rate(value: &str) -> Option<f64> {
    let (n, d) = value.split_once('/')?;
    let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
    (d != 0.0).then_some(n / d)
}

fn io_guard(output: &Path, inputs: &[PathBuf], overwrite: bool) -> Result<()> {
    crate::io::protect_inputs(output, inputs)?;
    ensure!(
        overwrite || !output.exists(),
        "output already exists; pass --overwrite"
    );
    Ok(())
}

// ---------------------------------------------------------------- cuts

/// Every timeline position where a clip starts or ends, excluding the ends.
fn cut_points(edit: &ValidatedEdit) -> Vec<(u64, Vec<String>)> {
    let mut cuts = std::collections::BTreeMap::<u64, Vec<String>>::new();
    let total = edit.duration_frames();
    let mut add = |frame: u64, what: String| {
        if frame > 0 && frame < total {
            cuts.entry(frame).or_default().push(what);
        }
    };
    for clip in edit.video_clips() {
        let name = file_name(&clip.source);
        add(
            clip.start_frames,
            format!("V{} in: {name} @ {:.3}s", clip.track + 1, clip.source_in),
        );
        add(
            clip.end_frames(),
            format!(
                "V{} out: {name} @ {:.3}s",
                clip.track + 1,
                clip.source_in + edit.fps().seconds(clip.duration_frames) * clip.speed
            ),
        );
    }
    for clip in edit.audio_clips() {
        let name = file_name(&clip.source);
        add(
            clip.start_frames,
            format!("A{} in: {name} @ {:.3}s", clip.track + 1, clip.source_in),
        );
        add(
            clip.end_frames(),
            format!(
                "A{} out: {name} @ {:.3}s",
                clip.track + 1,
                clip.source_in + edit.fps().seconds(clip.duration_frames) * clip.speed
            ),
        );
    }
    cuts.into_iter().collect()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("media")
        .to_owned()
}

fn cuts(args: &CutsArgs) -> Result<Value> {
    ensure!(
        args.handles.is_finite() && (0.2..=10.0).contains(&args.handles),
        "--handles must be 0.2..10 seconds"
    );
    let edit = timeline::load_and_validate(&args.edit)?;
    let points = cut_points(&edit);
    ensure!(!points.is_empty(), "the edit has no cuts to audition");
    ensure!(
        points.len() <= MAX_CUTS,
        "the edit has {} cuts; audition at most {MAX_CUTS} at a time",
        points.len()
    );
    let parent = args
        .output_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure!(parent.is_dir(), "output parent directory does not exist");
    std::fs::create_dir(&args.output_dir)
        .context("--output-dir must be a new directory (existing directories are never reused)")?;
    let dir = args.output_dir.canonicalize()?;
    let (render, _temporary) = match &args.render {
        Some(path) => {
            let info = media::probe(path)?;
            ensure!(
                info.has_video()
                    && (info.duration - edit.duration()).abs() < 0.1 + edit.fps().seconds(1),
                "--render must be a render of this edit ({:.3}s)",
                edit.duration()
            );
            (info.path, None)
        }
        None => {
            let temp = tempfile::Builder::new()
                .prefix(".skywrite-review-")
                .suffix(".mp4")
                .tempfile_in(&dir)?;
            let path = temp.path().to_owned();
            export::render(&edit, &path, true)?;
            (path, Some(temp))
        }
    };
    let fps = edit.fps();
    let mut clips = Vec::new();
    for (index, (frame, events)) in points.iter().enumerate() {
        let at = fps.seconds(*frame);
        let from = (at - args.handles).max(0.0);
        let to = (at + args.handles).min(edit.duration());
        let output = dir.join(format!("cut-{index:03}-{at:09.3}s.mp4"));
        let mut command = process::args(&[
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "error",
            "-n",
            "-ss",
            &format!("{from:.6}"),
            "-t",
            &format!("{:.6}", to - from),
            "-i",
        ]);
        command.push(render.clone().into_os_string());
        command.extend(process::args(&[
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "23",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "160k",
            "-movflags",
            "+faststart",
        ]));
        command.push(output.clone().into_os_string());
        process::run("ffmpeg", &command, Duration::from_secs(120), 1024 * 1024)?;
        clips.push(json!({"index":index,"cut_seconds":at,"cut_frame":frame,"clip":output,"clip_start_seconds":from,"cut_offset_in_clip":at-from,"events":events}));
    }
    Ok(json!({
        "output_dir": dir,
        "cut_count": clips.len(),
        "cuts": clips,
        "note": "Each clip plays the rendered edit around one cut. Watch and listen for clipped words, stumbles, repeated phrases, sound cut off mid-flow (applause, music tails), jump cuts and sync. Skywrite cannot judge delivery or emotional fit; that review is yours.",
    }))
}

// ---------------------------------------------------------------- lint

#[derive(Clone)]
struct Interval {
    start: f64,
    end: f64,
}

fn subtract(base: Interval, covers: &[Interval]) -> Vec<Interval> {
    let mut pieces = vec![base];
    for cover in covers {
        let mut next = Vec::new();
        for piece in pieces {
            if cover.end <= piece.start || cover.start >= piece.end {
                next.push(piece);
                continue;
            }
            if cover.start > piece.start {
                next.push(Interval {
                    start: piece.start,
                    end: cover.start,
                });
            }
            if cover.end < piece.end {
                next.push(Interval {
                    start: cover.end,
                    end: piece.end,
                });
            }
        }
        pieces = next;
    }
    pieces
}

fn normalize(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

const FILLERS: &[&str] = &["um", "uh", "umm", "uhh", "erm", "er", "ah", "hmm", "mm"];

struct TimelineWord<'a> {
    word: &'a Word,
    start: f64,
    end: f64,
    span: usize,
}

/// Mean square of a mono 16 kHz decode of a source range, in dBFS.
fn level(path: &Path, start: f64, duration: f64) -> Result<Option<f64>> {
    if duration <= 0.01 {
        return Ok(None);
    }
    let mut args = process::args(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-ss",
        &format!("{start:.6}"),
        "-t",
        &format!("{duration:.6}"),
        "-i",
    ]);
    args.push(path.as_os_str().to_owned());
    args.extend(process::args(&[
        "-map", "0:a:0", "-ac", "1", "-ar", "16000", "-f", "f32le", "pipe:1",
    ]));
    let out = process::run("ffmpeg", &args, Duration::from_secs(60), 8 * 1024 * 1024)?;
    let samples: Vec<f32> = out
        .stdout
        .chunks_exact(4)
        .map(|v| f32::from_le_bytes(v.try_into().expect("4 bytes")))
        .collect();
    if samples.len() < 160 {
        return Ok(None);
    }
    let mean = samples.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / samples.len() as f64;
    Ok(Some(10.0 * mean.max(1e-12).log10()))
}

fn lint(args: &LintArgs) -> Result<Value> {
    let edit = timeline::load_and_validate(&args.edit)?;
    let mut transcripts = HashMap::<PathBuf, Transcript>::new();
    for path in &args.transcript {
        let transcript = Transcript::load(path)?;
        let base = path.parent().unwrap_or(Path::new("."));
        let source = if transcript.source.is_absolute() {
            transcript.source.clone()
        } else {
            base.join(&transcript.source)
        };
        let source = media::canonical_file(&source)
            .with_context(|| format!("transcript {} source", path.display()))?;
        if transcripts.insert(source.clone(), transcript).is_some() {
            bail!("two transcripts were supplied for {}", source.display());
        }
    }
    let mut warnings = Vec::new();
    let fps = edit.fps();
    let spans = edit.audible_spans();

    // Map dialogue words into timeline time for every audible dialogue span.
    let mut words = Vec::new();
    for (index, span) in spans.iter().enumerate() {
        let Some(transcript) = transcripts.get(span.source) else {
            continue;
        };
        let source_end = span.source_in + span.duration * span.speed;
        for word in &transcript.words {
            if word.end <= span.source_in || word.start >= source_end {
                continue;
            }
            let start = span.start + (word.start.max(span.source_in) - span.source_in) / span.speed;
            let end = span.start + (word.end.min(source_end) - span.source_in) / span.speed;
            let mid = ((start + end) / 2.0 - span.start).clamp(0.0, span.duration);
            if span.mix.gain(mid, span.duration) > 0.0 {
                words.push(TimelineWord {
                    word,
                    start,
                    end,
                    span: index,
                });
            }
        }
    }
    words.sort_by(|a, b| a.start.total_cmp(&b.start));
    // A word split by a straight cut appears once per span; merge the halves so
    // it is not reported as a repetition.
    let frame = fps.seconds(1);
    let mut merged: Vec<TimelineWord> = Vec::with_capacity(words.len());
    for word in words {
        if let Some(last) = merged.last_mut()
            && std::ptr::eq(last.word, word.word)
            && (word.start - last.end).abs() <= frame * 1.5
        {
            last.end = last.end.max(word.end);
            continue;
        }
        merged.push(word);
    }
    let words = merged;

    // 1. A visible picture of someone speaking with no dialogue audible.
    let opaque: Vec<(usize, Interval)> = edit
        .video_clips()
        // Only full-frame opaque video hides the speaker; letterboxed pictures
        // and stills (often transparent titles) leave them visible.
        .filter(|c| {
            c.opacity >= 1.0
                && c.kind == MediaKind::Video
                && c.media.video().is_some_and(|v| {
                    let (w, h) = (v.width.unwrap_or(0) as f64, v.height.unwrap_or(0) as f64);
                    h > 0.0
                        && ((w / h) - edit.width as f64 / edit.height as f64).abs()
                            < 0.01 * edit.width as f64 / edit.height as f64
                })
        })
        .map(|c| {
            (
                c.track,
                Interval {
                    start: fps.seconds(c.start_frames) + c.fade_in,
                    end: fps.seconds(c.end_frames()) - c.fade_out,
                },
            )
        })
        .collect();
    for clip in edit.video_clips().filter(|c| c.kind == MediaKind::Video) {
        let Some(transcript) = transcripts.get(&clip.source) else {
            continue;
        };
        let start = fps.seconds(clip.start_frames);
        let duration = fps.seconds(clip.duration_frames);
        let covers: Vec<Interval> = opaque
            .iter()
            .filter(|(track, _)| *track > clip.track)
            .map(|(_, i)| i.clone())
            .collect();
        let visible = subtract(
            Interval {
                start,
                end: start + duration,
            },
            &covers,
        );
        let source_end = clip.source_in + duration * clip.speed;
        let mut silent = Vec::new();
        for word in &transcript.words {
            if word.end <= clip.source_in || word.start >= source_end {
                continue;
            }
            let t0 = start + (word.start.max(clip.source_in) - clip.source_in) / clip.speed;
            let t1 = start + (word.end.min(source_end) - clip.source_in) / clip.speed;
            let mid = (t0 + t1) / 2.0;
            if !visible.iter().any(|v| v.start <= mid && mid < v.end) {
                continue;
            }
            let heard = words.iter().any(|w| w.start < t1 && w.end > t0);
            if !heard {
                silent.push((t0, t1, word.text.as_str()));
            }
        }
        if !silent.is_empty() {
            let text: Vec<&str> = silent.iter().take(12).map(|s| s.2).collect();
            warnings.push(json!({
                "kind": "visible_speaker_without_dialogue",
                "timeline_start": silent[0].0,
                "timeline_end": silent[silent.len() - 1].1,
                "clip": format!("video track {} clip {}", clip.track, clip.index),
                "words": silent.len(),
                "message": format!(
                    "{} is visible speaking ({:?}…) from {:.2}s to {:.2}s, but no dialogue source is audible then. Viewers will see a silent speaker; start the picture at the dialogue, cover it with B-roll, or make its sound audible.",
                    file_name(&clip.source), text.join(" "), silent[0].0, silent[silent.len()-1].1
                ),
            }));
        }
    }

    // 2. Repeated words, across or inside cuts, and filler words in selects.
    for pair in words.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let same = normalize(&a.word.text);
        if same.is_empty() || same != normalize(&b.word.text) || b.start - a.end > 1.5 {
            continue;
        }
        let across = a.span != b.span;
        warnings.push(json!({
            "kind": if across {"repeated_word_across_cut"} else {"repeated_word_in_select"},
            "timeline_start": a.start,
            "timeline_end": b.end,
            "message": format!(
                "\"{}\" is heard twice in a row at {:.2}s and {:.2}s{}. Listen: this is often a stumble or restart; trim one if unintended.",
                a.word.text.trim(), a.start, b.start,
                if across {" across a cut"} else {" inside one select"}
            ),
        }));
    }
    let fillers: Vec<Value> = words
        .iter()
        .filter(|w| FILLERS.contains(&normalize(&w.word.text).as_str()))
        .take(64)
        .map(|w| json!({"word":w.word.text,"timeline_start":w.start}))
        .collect();
    if !fillers.is_empty() {
        warnings.push(json!({
            "kind": "filler_words",
            "count": fillers.len(),
            "items": fillers,
            "message": "Filler words were transcribed inside audible dialogue. Keep or trim them deliberately. Transcription often omits fillers, so their absence does not prove clean delivery.",
        }));
    }

    // 2b. Words clipped by a cut: a transcribed word straddles a clip's in or
    // out point with no straight continuation and no fade to soften it.
    let slack = frame.max(0.04);
    let mut clipped = Vec::new();
    for span in &spans {
        let Some(transcript) = transcripts.get(span.source) else {
            continue;
        };
        let tolerance = slack * span.speed;
        let source_out = span.source_in + span.duration * span.speed;
        let end = span.start + span.duration;
        let continued_after = spans.iter().any(|o| {
            o.source == span.source
                && (o.source_in - source_out).abs() <= tolerance * 1.01
                && (o.start - end).abs() <= frame * 1.01
        });
        let continued_before = spans.iter().any(|o| {
            o.source == span.source
                && (o.source_in + o.duration * o.speed - span.source_in).abs() <= tolerance * 1.01
                && (o.start + o.duration - span.start).abs() <= frame * 1.01
        });
        for word in &transcript.words {
            let straddles = |at: f64| word.start < at - tolerance && word.end > at + tolerance;
            if !continued_before
                && span.mix.fade_in < 0.1
                && span.mix.gain(0.0, span.duration) > 0.0
                && straddles(span.source_in)
            {
                clipped.push(json!({"edge":"in","word":word.text,"timeline_seconds":span.start,"source_seconds":span.source_in,"clip":span.label}));
            }
            if !continued_after
                && span.mix.fade_out < 0.1
                && span
                    .mix
                    .gain(span.duration, span.duration)
                    .max(span.mix.envelope(span.duration))
                    > 0.0
                && straddles(source_out)
            {
                clipped.push(json!({"edge":"out","word":word.text,"timeline_seconds":end,"source_seconds":source_out,"clip":span.label}));
            }
        }
    }
    if !clipped.is_empty() {
        clipped.sort_by(|a, b| {
            a["timeline_seconds"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&b["timeline_seconds"].as_f64().unwrap_or(0.0))
        });
        warnings.push(json!({
            "kind": "word_clipped_at_cut",
            "count": clipped.len(),
            "items": clipped.into_iter().take(64).collect::<Vec<_>>(),
            "message": "A cut falls inside a transcribed word, so part of the word is lost. Listen with skywrite review cuts; move the cut to a pause, extend the clip, or add a short audio fade if it is intended.",
        }));
    }

    // 3 and 4 need decoded audio levels.
    let mut measurements = 0usize;
    let mut levels_checked = false;
    if !args.no_levels {
        levels_checked = true;
        // Within one output frame: frame-rounded durations at NTSC rates leave
        // millisecond differences across a straight split.
        let frame = fps.seconds(1);
        for span in &spans {
            if measurements + 2 > MAX_MEASUREMENTS {
                break;
            }
            let source_out = span.source_in + span.duration * span.speed;
            let continues = spans.iter().any(|o| {
                o.source == span.source
                    && (o.source_in - source_out).abs() <= frame * span.speed.max(o.speed) * 1.01
                    && (o.start - (span.start + span.duration)).abs() <= frame * 1.01
            });
            let duration = media::probe_for_timing(span.source)?.duration;
            if !continues && span.mix.fade_out == 0.0 && source_out + 0.3 < duration {
                measurements += 2;
                let before = level(span.source, source_out - 0.4 * span.speed, 0.4 * span.speed)?;
                let after = level(span.source, source_out, 0.6 * span.speed)?;
                // Judge what the mix would have played: apply the clip's gain at the cut.
                let gain = span
                    .mix
                    .gain(span.duration, span.duration)
                    .max(span.mix.envelope(span.duration));
                let gain_db = 20.0 * gain.max(1e-6).log10();
                if let (Some(before), Some(after)) =
                    (before.map(|v| v + gain_db), after.map(|v| v + gain_db))
                    && after > -32.0
                    && after > before - 6.0
                {
                    let at = span.start + span.duration;
                    warnings.push(json!({
                        "kind": "sound_cut_mid_flow",
                        "timeline_start": at,
                        "source": span.source,
                        "source_seconds": source_out,
                        "level_before_dbfs": before,
                        "level_after_dbfs": after,
                        "message": format!(
                            "{} ends at {:.2}s while its source keeps sounding ({:.0} dBFS in the next 0.6s, {:.0} dBFS just before). This is often applause, laughter or a music tail cut off abruptly; listen, then extend, fade out or overlap it.",
                            span.label, at, after, before
                        ),
                    }));
                }
            }
        }
        // Beds: audible non-dialogue sound overlapping dialogue words.
        // A declared track role decides; otherwise a source without a transcript is a bed.
        let is_bed = |s: &&timeline::AudibleSpan| match s.role {
            Some(role) => role != timeline::Role::Dialogue,
            None => !transcripts.contains_key(s.source),
        };
        for bed in spans.iter().filter(is_bed) {
            let overlapping: Vec<&TimelineWord> = words
                .iter()
                .filter(|w| w.start >= bed.start && w.end <= bed.start + bed.duration)
                .collect();
            if overlapping.is_empty() {
                continue;
            }
            // Measure up to eight windows of dialogue, spread through the overlap.
            let step = (overlapping.len() / 8).max(1);
            for word in overlapping.iter().step_by(step).take(8) {
                if measurements + 2 > MAX_MEASUREMENTS {
                    break;
                }
                measurements += 2;
                let dialogue = &spans[word.span];
                let window = (word.end - word.start).clamp(0.15, 1.0);
                let d_at = dialogue.source_in + (word.start - dialogue.start) * dialogue.speed;
                let b_at = bed.source_in + (word.start - bed.start) * bed.speed;
                let (Some(d), Some(b)) = (
                    level(dialogue.source, d_at, window * dialogue.speed)?,
                    level(bed.source, b_at, window * bed.speed)?,
                ) else {
                    continue;
                };
                let d_gain = dialogue
                    .mix
                    .gain(word.start - dialogue.start, dialogue.duration);
                let b_gain = bed.mix.gain(word.start - bed.start, bed.duration);
                if d_gain <= 0.0 || b_gain <= 0.0 {
                    continue;
                }
                let d = d + 20.0 * d_gain.log10();
                let b = b + 20.0 * b_gain.log10();
                if b > d - 10.0 {
                    warnings.push(json!({
                        "kind": "bed_close_to_dialogue",
                        "timeline_start": word.start,
                        "bed": bed.label,
                        "dialogue_dbfs": d,
                        "bed_dbfs": b,
                        "message": format!(
                            "At {:.2}s {} is {:.0} dB against dialogue at {:.0} dB (within 10 dB). Speech may be hard to follow; listen, and consider volume keyframes on the bed.",
                            word.start, bed.label, b, d
                        ),
                    }));
                    break;
                }
            }
        }
    }
    warnings.sort_by(|a, b| {
        a["timeline_start"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&b["timeline_start"].as_f64().unwrap_or(0.0))
    });
    Ok(json!({
        "edit": args.edit,
        "warning_count": warnings.len(),
        "warnings": warnings,
        "checked": {
            "dialogue_sources": transcripts.keys().collect::<Vec<_>>(),
            "dialogue_words_heard": words.len(),
            "levels": levels_checked,
            "level_measurements": measurements,
        },
        "limitations": [
            "Checks are objective signals, not editorial judgement: they cannot assess delivery, emotional fit, B-roll relevance or music choice.",
            "Speaker and repetition checks only cover sources whose transcripts were supplied, and only as well as those transcripts capture what was said.",
            "Level checks measure short windows of the source audio with the edit's gains applied; they do not replace listening to the mix (skywrite review cuts)."
        ],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interval_subtraction_and_word_normalization() {
        let pieces = subtract(
            Interval {
                start: 0.0,
                end: 10.0,
            },
            &[
                Interval {
                    start: 2.0,
                    end: 4.0,
                },
                Interval {
                    start: 8.0,
                    end: 12.0,
                },
            ],
        );
        let pairs: Vec<(f64, f64)> = pieces.iter().map(|p| (p.start, p.end)).collect();
        assert_eq!(pairs, vec![(0.0, 2.0), (4.0, 8.0)]);
        assert_eq!(normalize("Evanston,"), "evanston");
        assert_eq!(normalize(" UM… "), "um");
    }
}
