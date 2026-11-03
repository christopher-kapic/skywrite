//! Model descriptions of exact frames and short video ranges.
//!
//! Frames are described independently, one request per frame, so each result
//! is cached by what it depends on: source content hash, exact presentation
//! time, image settings, model, prompt version and cast. A coarse survey and
//! a later dense "focus in" pass around a cut share every frame they have in
//! common, and nothing is paid for twice. Requests use the data policy in
//! `openrouter::data_policy` (zero retention by default).
use crate::{
    media, openrouter,
    review::{self, FrameSelection},
    transcript::Transcript,
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

pub const FRAME_PROMPT_VERSION: &str = "frame-describe-v1";
pub const VIDEO_PROMPT_VERSION: &str = "video-describe-v1";
const FRAME_FIT: u32 = 768;
const JPEG_QUALITY: &str = "4";
const DEFAULT_MAX_FRAMES: usize = 600;
const HARD_MAX_FRAMES: usize = 6000;
const MAX_VIDEO_SECONDS: f64 = 600.0;
const MAX_VIDEO_BYTES: u64 = 17 * 1024 * 1024;

#[derive(Subcommand)]
pub enum DescribeCommand {
    /// Describe exact frames independently (cached per frame): a coarse survey or a dense pass around a cut.
    Frames(FrameArgs),
    /// Describe motion, expressions, gestures and shot changes in a short source range with a video-input model.
    Video(VideoArgs),
}

#[derive(Args)]
pub struct FrameArgs {
    pub source: PathBuf,
    /// Range start in source seconds (from the first presented sample).
    #[arg(long, default_value_t = 0.0)]
    pub start: f64,
    /// Range length in seconds; defaults to the rest of the source.
    #[arg(long)]
    pub duration: Option<f64>,
    /// Center a dense window on this source time instead of --start/--duration.
    #[arg(long, conflicts_with_all = ["start", "duration"])]
    pub around: Option<f64>,
    /// Window length for --around, in seconds.
    #[arg(long, default_value_t = 1.0, requires = "around")]
    pub window: f64,
    /// Describe every Nth decoded frame (30 = about once a second at 30 fps; 2 = dense).
    #[arg(long, conflicts_with = "interval")]
    pub every: Option<u32>,
    /// Describe one frame per this many seconds.
    #[arg(long)]
    pub interval: Option<f64>,
    /// Optional cast file of TEXT appearance descriptions for advisory "who is visible" matching.
    #[arg(long)]
    pub cast: Option<PathBuf>,
    /// Attach words active at each frame from this word-timed transcript (not sent to the model).
    #[arg(long)]
    pub transcript: Option<PathBuf>,
    #[arg(long)]
    pub model: Option<String>,
    /// Refuse to describe more uncached frames than this (default 600).
    #[arg(long, default_value_t = DEFAULT_MAX_FRAMES)]
    pub max_frames: usize,
    /// Parallel requests, 1..16.
    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,
    /// Extract frames and report how many are cached; make no requests.
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub overwrite: bool,
}

#[derive(Args)]
pub struct VideoArgs {
    pub source: PathBuf,
    #[arg(long, default_value_t = 0.0)]
    pub start: f64,
    #[arg(long)]
    pub duration: f64,
    /// Encoded height for the request (smaller is cheaper; 240..1080).
    #[arg(long, default_value_t = 480)]
    pub height: u32,
    #[arg(long)]
    pub cast: Option<PathBuf>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CastMember {
    pub name: String,
    pub description: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Cast {
    pub version: u32,
    pub people: Vec<CastMember>,
}

pub fn load_cast(path: &Path) -> Result<Cast> {
    let cast: Cast = crate::io::read_json(path)?;
    ensure!(cast.version == 1, "cast files use \"version\": 1");
    ensure!(
        (1..=32).contains(&cast.people.len()),
        "a cast lists 1..32 people"
    );
    let mut names = std::collections::HashSet::new();
    for person in &cast.people {
        ensure!(
            !person.name.trim().is_empty()
                && person.name.len() <= 64
                && !person.name.chars().any(char::is_control),
            "cast names are 1..64 bytes without control characters"
        );
        ensure!(
            !person.description.trim().is_empty() && person.description.len() <= 512,
            "cast descriptions are 1..512 bytes"
        );
        ensure!(
            person.name != "unknown" && names.insert(person.name.to_lowercase()),
            "cast names must be unique and not \"unknown\""
        );
    }
    Ok(cast)
}

const FRAME_PROMPT: &str = r#"You describe ONE exact video frame for a video editor's searchable log. Describe only what is visible in this single still image. Do not guess names, identities, off-screen events, or what happened before/after. Do not invent text, objects, or emotions; describe facial expression only from visible cues. If something is unclear, write "unclear" instead of guessing.

Return only a JSON object with exactly these keys:
{
  "shot_size": "extreme_close_up|close_up|medium_close_up|medium|medium_wide|wide|extreme_wide|insert|empty",
  "camera_angle": "eye_level|high|low|overhead|unclear",
  "people_count": integer,
  "people": [
    {
      "position": "where in frame (e.g. left, center, right, background)",
      "appearance": "visible clothing, hair, accessories",
      "framing": "how much of the person is visible (e.g. head and shoulders, partially cut off)",
      "gaze": "where they are looking, relative to the camera",
      "expression": "neutral|smiling|laughing|frowning|concentrating|surprised|unclear|other: short",
      "mouth": "closed|open|partly_open|not_visible",
      "appears_mid_speech": true|false|null,
      "gesture_or_action": "visible hand/body action, or none"
    }
  ],
  "setting": "short description of the location",
  "notable_objects": ["visible objects only"],
  "visible_text": ["exact legible text only; empty if none"],
  "image_issues": ["e.g. motion_blur, low_resolution, dark, compression; empty if none"],
  "summary": "one factual sentence"
}"#;

const CAST_SECTION: &str = r#"

Also match visible people against this cast list of TEXT appearance descriptions supplied by the user (there are no reference photos). Judge only from visible clothing, hair, accessories, position and setting. Do not infer identity from faces, and do not assume a listed person is present. Entries may be similar: choose one only when the deciding feature is actually visible and fits it better than every other entry; otherwise answer "unknown". Add these keys to the JSON object:
  "cast_matches": [{"position": "where in frame", "match": "exact cast name or unknown", "deciding_feature_seen": "the visible cue that decided it", "runner_up": "second most likely cast name or none"}],
  "cast_present": ["cast names clearly visible"]

Cast list:
"#;

fn frame_prompt(cast: Option<&Cast>) -> String {
    let mut prompt = FRAME_PROMPT.to_owned();
    if let Some(cast) = cast {
        prompt.push_str(CAST_SECTION);
        for person in &cast.people {
            prompt.push_str(&format!("- {}: {}\n", person.name, person.description));
        }
    }
    prompt
}

const VIDEO_PROMPT: &str = r#"You describe a short video clip for a video editor. Describe what actually happens over time: who is visible (by appearance, not by name), their motion, gestures, head turns, gaze changes, facial expressions and when they change, whether mouths move as if speaking, camera movement, and any cuts or shot changes. Give approximate timestamps in seconds from the clip start. Only report what is visible; write "unclear" rather than guessing. Do not describe audio.

Return only a JSON object:
{
  "duration_seconds_estimate": number,
  "shots": [{"start": number, "end": number, "shot_size": "...", "camera_motion": "static|pan|tilt|zoom|handheld|unclear", "description": "..."}],
  "events": [{"time": number, "who": "appearance-based label", "event": "visible action, gesture, expression change, or speech onset"}],
  "people": [{"label": "appearance-based label", "appearance": "...", "speaking_visibly": "yes|no|unclear"}],
  "summary": "two factual sentences at most"
}"#;

/// Cache key for one frame description; public so tests can seed the cache.
pub fn frame_cache_key(
    source_sha256: &str,
    time: f64,
    model: &str,
    cast: Option<&Cast>,
) -> Result<String> {
    let cast = serde_json::to_vec(&cast)?;
    Ok(openrouter::cache_key(&[
        FRAME_PROMPT_VERSION.as_bytes(),
        source_sha256.as_bytes(),
        format!("{time:.6}").as_bytes(),
        format!("fit{FRAME_FIT}-q{JPEG_QUALITY}").as_bytes(),
        model.as_bytes(),
        &cast,
    ]))
}

#[derive(Serialize, Deserialize)]
struct CachedDescription {
    version: u32,
    model: String,
    description: Value,
    usage: Value,
}

fn read_cached(path: &Path) -> Option<CachedDescription> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_cached(path: &Path, entry: &CachedDescription) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match crate::io::atomic_write(path, &serde_json::to_vec(entry)?, false) {
        Ok(()) => Ok(()),
        Err(_) if path.is_file() => Ok(()),
        Err(error) => Err(error),
    }
}

/// One request; disables reasoning where allowed (a per-frame task does not
/// need it), and repeats without that setting when a model requires reasoning.
/// A rejected request is not billed, so this cannot double-charge.
fn ask(
    api: &openrouter::OpenRouter,
    model: &str,
    content: Value,
    max_tokens: u32,
) -> Result<(Value, Value)> {
    let mut request = json!({"model":model,"messages":[{"role":"user","content":content}],
        "response_format":{"type":"json_object"},"temperature":0.2,"max_tokens":max_tokens,
        "reasoning":{"enabled":false}});
    let response = match api.request("chat/completions", &request) {
        Ok(response) => response,
        Err(error) if error.to_string().contains("Reasoning is mandatory") => {
            request.as_object_mut().expect("object").remove("reasoning");
            api.request("chat/completions", &request)?
        }
        Err(error) => return Err(error),
    };
    let description = openrouter::assistant_json(&response.body)?;
    Ok((
        description,
        response.body.get("usage").cloned().unwrap_or(Value::Null),
    ))
}

fn fit_width(info: &media::MediaInfo) -> u32 {
    let video = info.video();
    let (w, h) = (
        video.and_then(|v| v.width).unwrap_or(16) as f64,
        video.and_then(|v| v.height).unwrap_or(9) as f64,
    );
    let width = if w >= h {
        FRAME_FIT as f64
    } else {
        FRAME_FIT as f64 * w / h
    };
    ((width / 2.0).round() as u32 * 2).clamp(2, FRAME_FIT)
}

fn cost(usage: &Value) -> f64 {
    usage["cost"].as_f64().unwrap_or(0.0)
}

pub fn execute(command: DescribeCommand) -> Result<Value> {
    match command {
        DescribeCommand::Frames(args) => frames(&args),
        DescribeCommand::Video(args) => video(&args),
    }
}

fn finish(value: Value, output: Option<&Path>, overwrite: bool) -> Result<Value> {
    if let Some(path) = output {
        crate::io::write_json(path, &value, overwrite)?;
    }
    Ok(value)
}

fn frames(args: &FrameArgs) -> Result<Value> {
    ensure!(
        (1..=16).contains(&args.concurrency),
        "--concurrency must be 1..16"
    );
    ensure!(
        (1..=HARD_MAX_FRAMES).contains(&args.max_frames),
        "--max-frames must be 1..{HARD_MAX_FRAMES}"
    );
    let info = media::probe_for_timing(&args.source)?;
    if let Some(output) = &args.output {
        crate::io::protect_inputs(output, std::slice::from_ref(&info.path))?;
        ensure!(
            args.overwrite || !output.exists(),
            "output already exists; pass --overwrite"
        );
    }
    let (start, duration) = match args.around {
        Some(center) => {
            ensure!(
                args.window.is_finite() && (0.04..=60.0).contains(&args.window),
                "--window must be 0.04..60 seconds"
            );
            let start = (center - args.window / 2.0).max(0.0);
            (
                start,
                (center + args.window / 2.0).min(info.duration) - start,
            )
        }
        None => (
            args.start,
            args.duration.unwrap_or(info.duration - args.start),
        ),
    };
    let every = match (args.every, args.interval, args.around) {
        (None, None, Some(_)) => Some(2),
        (None, None, None) => bail!("choose --every N, --interval SECONDS, or --around TIME"),
        (every, _, _) => every,
    };
    let cast = args.cast.as_deref().map(load_cast).transpose()?;
    let transcript = args
        .transcript
        .as_deref()
        .map(Transcript::load)
        .transpose()?;
    let model = crate::models::resolve_model("frame-description", args.model.as_deref())?;
    let root = crate::cache::cache_root(args.cache_dir.as_deref())?;
    eprintln!("Hashing source for the description cache…");
    let source_sha256 = media::file_sha256(&info.path)?;
    let image_dir = root
        .join("assets")
        .join(&source_sha256)
        .join("exact-frames");
    std::fs::create_dir_all(&image_dir)?;
    let temp = tempfile::Builder::new()
        .prefix("skywrite-describe-")
        .tempdir()?;
    let extracted = review::extract_frames(
        &info,
        &FrameSelection {
            start,
            duration,
            every,
            interval: args.interval,
            width: fit_width(&info),
        },
        temp.path(),
        HARD_MAX_FRAMES,
    )?;
    let descriptions = root.join("frame-descriptions");
    // Persist frame images by exact time so agents can view what was described.
    let mut work = Vec::new();
    for frame in &extracted {
        let key = frame_cache_key(&source_sha256, frame.time, &model, cast.as_ref())?;
        let image = image_dir.join(format!("{:015.6}-fit{FRAME_FIT}.jpg", frame.time));
        if !image.is_file() {
            std::fs::copy(&frame.path, &image)?;
        }
        work.push((frame.time, image, descriptions.join(format!("{key}.json"))));
    }
    let uncached = work
        .iter()
        .filter(|(_, _, cache)| read_cached(cache).is_none())
        .count();
    let fps = info
        .video()
        .and_then(|v| v.avg_frame_rate.as_deref())
        .and_then(|r| {
            let (n, d) = r.split_once('/')?;
            let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
            (d > 0.0 && n > 0.0).then_some(n / d)
        });
    let video_start = info
        .video()
        .and_then(|v| v.start_time)
        .unwrap_or(info.start_time)
        - info.start_time;
    if args.dry_run {
        return finish(
            json!({"source":info.path,"model":model,"frames":work.len(),"cached":work.len()-uncached,"to_describe":uncached,
                "range":{"start":start,"duration":duration},"requests":0,
                "note":"Dry run: frames were extracted locally; no model requests were made."}),
            args.output.as_deref(),
            args.overwrite,
        );
    }
    ensure!(
        uncached <= args.max_frames,
        "{uncached} frames need descriptions, above --max-frames {}; narrow the range, sample less often, or raise --max-frames deliberately (see --dry-run)",
        args.max_frames
    );
    let api = openrouter::OpenRouter::new(None)?;
    let prompt = frame_prompt(cast.as_ref());
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Value>>> = Mutex::new(vec![None; work.len()]);
    let spent = Mutex::new(0.0f64);
    let done = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..args.concurrency.min(work.len().max(1)) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some((time, image, cache)) = work.get(index) else {
                        break;
                    };
                    let result = (|| -> Result<Value> {
                        if let Some(entry) = read_cached(cache) {
                            return Ok(json!({"description":entry.description,"cached":true}));
                        }
                        let bytes = std::fs::read(image)?;
                        let content = json!([{"type":"text","text":prompt},
                            {"type":"image_url","image_url":{"url":format!("data:image/jpeg;base64,{}",STANDARD.encode(bytes))}}]);
                        let (description, usage) = ask(&api, &model, content, 1500)?;
                        *spent.lock().expect("lock") += cost(&usage);
                        write_cached(cache, &CachedDescription {
                            version: 1,
                            model: model.clone(),
                            description: description.clone(),
                            usage: usage.clone(),
                        })?;
                        Ok(json!({"description":description,"cached":false,"cost_usd":usage.get("cost")}))
                    })();
                    let value = match result {
                        Ok(mut value) => {
                            value["source_time"] = json!(time);
                            value
                        }
                        Err(error) => json!({"source_time":time,"error":format!("{error:#}")}),
                    };
                    results.lock().expect("lock")[index] = Some(value);
                    let finished = done.fetch_add(1, Ordering::SeqCst) + 1;
                    if finished.is_multiple_of(25) || finished == work.len() {
                        eprintln!("Described {finished}/{} frames", work.len());
                    }
                }
            });
        }
    });
    let mut frames = Vec::new();
    let (mut cached, mut failed) = (0usize, 0usize);
    for ((time, image, _), value) in work.iter().zip(results.into_inner().expect("lock")) {
        let mut value = value.context("frame result missing")?;
        if value["cached"] == true {
            cached += 1;
        }
        if value.get("error").is_some() {
            failed += 1;
        }
        value["image"] = json!(image);
        value["frame"] = json!(fps.map(|fps| ((time - video_start) * fps).round() as i64));
        if let Some(transcript) = &transcript {
            let words: Vec<&str> = transcript
                .words
                .iter()
                .filter(|w| w.start - 0.25 <= *time && *time <= w.end + 0.25)
                .map(|w| w.text.as_str())
                .collect();
            value["words_near_frame"] = json!(words);
        }
        frames.push(value);
    }
    ensure!(
        failed < frames.len() || frames.is_empty(),
        "every frame description failed; first error: {}",
        frames[0]["error"]
    );
    let spent = spent.into_inner().expect("lock");
    finish(
        json!({
            "source": info.path,
            "source_sha256": source_sha256,
            "model": model,
            "prompt_version": FRAME_PROMPT_VERSION,
            "data_policy": openrouter::data_policy()?,
            "range": {"start": start, "duration": duration, "every": every, "interval": args.interval},
            "counts": {"frames": frames.len(), "cached": cached, "described": frames.len() - cached - failed, "failed": failed},
            "cost_usd": spent,
            "cast": cast.as_ref().map(|c| c.people.iter().map(|p| &p.name).collect::<Vec<_>>()),
            "frames": frames,
            "notes": [
                "Each frame is described on its own from one still image; motion and timing between frames are not observed (use describe video or review sheet).",
                "Descriptions are cached per exact frame, model, prompt and cast; repeating or overlapping passes reuse them at no cost. Rerun the same command to retry failed frames only.",
                "cast_matches are advisory text-description matches, not identification. They fail when the deciding feature is not visible or people look alike; confirm by viewing the frame."
            ],
        }),
        args.output.as_deref(),
        args.overwrite,
    )
}

fn video(args: &VideoArgs) -> Result<Value> {
    ensure!(
        args.duration.is_finite() && args.duration > 0.0 && args.duration <= MAX_VIDEO_SECONDS,
        "--duration must be in (0, {MAX_VIDEO_SECONDS}] seconds"
    );
    ensure!(
        (240..=1080).contains(&args.height),
        "--height must be 240..1080"
    );
    let info = media::probe_for_timing(&args.source)?;
    ensure!(info.has_video(), "source has no video stream");
    media::validate_range(&info, args.start, args.duration)?;
    if let Some(output) = &args.output {
        crate::io::protect_inputs(output, std::slice::from_ref(&info.path))?;
        ensure!(
            args.overwrite || !output.exists(),
            "output already exists; pass --overwrite"
        );
    }
    let cast = args.cast.as_deref().map(load_cast).transpose()?;
    let model = crate::models::resolve_model("video-description", args.model.as_deref())?;
    let root = crate::cache::cache_root(args.cache_dir.as_deref())?;
    let source_sha256 = media::file_sha256(&info.path)?;
    let encode = format!("h264-crf28-h{}-noaudio", args.height);
    let key = openrouter::cache_key(&[
        VIDEO_PROMPT_VERSION.as_bytes(),
        source_sha256.as_bytes(),
        format!("{:.6}:{:.6}", args.start, args.duration).as_bytes(),
        encode.as_bytes(),
        model.as_bytes(),
        &serde_json::to_vec(&cast)?,
    ]);
    let cache = root.join("video-descriptions").join(format!("{key}.json"));
    let shift = |mut description: Value| {
        // Model times are clip-relative; add source times alongside them.
        for list in ["shots", "events"] {
            if let Some(items) = description[list].as_array_mut() {
                for item in items {
                    for field in ["start", "end", "time"] {
                        if let Some(t) = item[field].as_f64() {
                            item[format!("source_{field}")] = json!(args.start + t);
                        }
                    }
                }
            }
        }
        description
    };
    let base = json!({"source":info.path,"model":model,"prompt_version":VIDEO_PROMPT_VERSION,
        "range":{"start":args.start,"duration":args.duration},"encoding":encode,"data_policy":openrouter::data_policy()?,
        "notes":["Times in the model output are approximate clip-relative seconds; source_* fields add the range start.","Descriptions are model observations of a reduced-resolution clip; confirm important moments with review sheet."]});
    if let Some(entry) = read_cached(&cache) {
        let mut value = base;
        value["description"] = shift(entry.description);
        value["cached"] = json!(true);
        value["cost_usd"] = json!(0.0);
        return finish(value, args.output.as_deref(), args.overwrite);
    }
    if args.dry_run {
        let mut value = base;
        value["cached"] = json!(false);
        value["note"] = json!("Dry run: not cached; no request made.");
        return finish(value, args.output.as_deref(), args.overwrite);
    }
    let temp = tempfile::Builder::new()
        .prefix("skywrite-video-")
        .tempdir()?;
    let clip = temp.path().join("clip.mp4");
    let video_height = info.video().and_then(|v| v.height).unwrap_or(args.height);
    let mut command = crate::process::args(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-ss",
        &format!("{:.6}", args.start),
        "-t",
        &format!("{:.6}", args.duration),
        "-i",
    ]);
    command.push(info.path.clone().into_os_string());
    command.extend(crate::process::args(&[
        "-map",
        "0:v:0",
        "-an",
        "-vf",
        &format!("scale=-2:{}", args.height.min(video_height)),
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "28",
        "-pix_fmt",
        "yuv420p",
        "-movflags",
        "+faststart",
    ]));
    command.push(clip.clone().into_os_string());
    crate::process::run("ffmpeg", &command, Duration::from_secs(600), 1024 * 1024)?;
    let size = std::fs::metadata(&clip)?.len();
    ensure!(
        size <= MAX_VIDEO_BYTES,
        "encoded clip is {size} bytes; inline requests are limited to {MAX_VIDEO_BYTES}. Shorten --duration or lower --height"
    );
    let mut prompt = VIDEO_PROMPT.to_owned();
    if let Some(cast) = &cast {
        prompt.push_str("\n\nPeople may match this cast of TEXT appearance descriptions (advisory; say unknown unless the deciding feature is visible). Use a cast name as \"who\" only when clear:\n");
        for person in &cast.people {
            prompt.push_str(&format!("- {}: {}\n", person.name, person.description));
        }
    }
    let content = json!([{"type":"text","text":prompt},
        {"type":"video_url","video_url":{"url":format!("data:video/mp4;base64,{}",STANDARD.encode(std::fs::read(&clip)?))}}]);
    let api = openrouter::OpenRouter::new(None)?;
    let (description, usage) = ask(&api, &model, content, 2500)?;
    write_cached(
        &cache,
        &CachedDescription {
            version: 1,
            model: model.clone(),
            description: description.clone(),
            usage: usage.clone(),
        },
    )?;
    let mut value = base;
    value["description"] = shift(description);
    value["cached"] = json!(false);
    value["cost_usd"] = json!(cost(&usage));
    value["encoded_bytes"] = json!(size);
    finish(value, args.output.as_deref(), args.overwrite)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_keys_depend_on_frame_model_and_cast_only() {
        let cast = Cast {
            version: 1,
            people: vec![CastMember {
                name: "Ralph".into(),
                description: "grey suit".into(),
            }],
        };
        let a = frame_cache_key("ab", 1.0, "m", None).unwrap();
        assert_eq!(a, frame_cache_key("ab", 1.0000001, "m", None).unwrap());
        assert_ne!(a, frame_cache_key("ab", 1.04, "m", None).unwrap());
        assert_ne!(a, frame_cache_key("ab", 1.0, "n", None).unwrap());
        assert_ne!(a, frame_cache_key("ab", 1.0, "m", Some(&cast)).unwrap());
        assert!(frame_prompt(Some(&cast)).contains("- Ralph: grey suit"));
    }
}
