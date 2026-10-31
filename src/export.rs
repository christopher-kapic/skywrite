//! Deterministic output backends compiled from a validated edit.
use crate::{
    captions::{self, CaptionMode},
    io, media, process,
    timeline::{MediaKind, ValidatedEdit},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fmt::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};
mod fcpxml;
mod project;

pub fn export(edit: &ValidatedEdit, target: &str, output: &Path, overwrite: bool) -> Result<Value> {
    prevent_source_overwrite(edit, output)?;
    match target {
        "fcpxml" => io::atomic_write(output, fcpxml::compile(edit)?.as_bytes(), overwrite)?,
        "ffmpeg" | "python" => {
            io::atomic_write(output, python_script(edit)?.as_bytes(), overwrite)?
        }
        "hyperframes" | "remotion" => return project::export(edit, target, output),
        _ => bail!(
            "unsupported export target {target}; choose fcpxml, ffmpeg, hyperframes, or remotion"
        ),
    }
    let mut warnings = edit.warnings.clone();
    if target == "fcpxml" && !edit.captions.is_empty() {
        warnings.push("Caption cues are preserved as plain JSON in the assembly note; import caption tracks separately. FCPXML does not apply burn-in. Final Cut import has not been tested.".into());
    }
    Ok(
        json!({"target":target,"output":output,"duration_seconds":edit.duration(),"duration_frames":edit.duration_frames,"warnings":warnings}),
    )
}
fn prevent_source_overwrite(edit: &ValidatedEdit, path: &Path) -> Result<()> {
    io::protect_inputs(
        path,
        &edit
            .protected_paths()
            .map(Path::to_owned)
            .collect::<Vec<_>>(),
    )
}
#[derive(Debug, Serialize)]
struct RenderPlan {
    args: Vec<String>,
    sources: Vec<PathBuf>,
    duration_seconds: f64,
    timeout_seconds: u64,
    sidecars: Vec<String>,
}
fn number(value: f64) -> String {
    format!("{value:.9}")
}
fn push(args: &mut Vec<String>, items: &[&str]) {
    args.extend(items.iter().map(|s| (*s).to_owned()));
}
fn tempo(speed: f64) -> String {
    let mut remaining = speed;
    let mut parts = Vec::new();
    while remaining > 2.0 {
        parts.push("atempo=2".into());
        remaining /= 2.0;
    }
    while remaining < 0.5 {
        parts.push("atempo=0.5".into());
        remaining /= 0.5;
    }
    parts.push(format!("atempo={}", number(remaining)));
    parts.join(",")
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unseeked_prefix(info: &media::MediaInfo, source_in: f64) -> f64 {
    let before_video = info
        .video()
        .and_then(|video| video.start_time)
        .is_some_and(|start| source_in < start - info.start_time);
    // TS has no reliable random-access index; input seeking can miss the
    // only usable keyframe. This affects rendering, never metadata inspection.
    if before_video || info.format_name.split(',').any(|name| name == "mpegts") {
        source_in
    } else {
        0.0
    }
}

fn audio_segment(
    input: usize,
    info: &media::MediaInfo,
    source_times: (f64, f64),
    duration: f64,
    speed: f64,
    volume: f64,
    label: &str,
) -> String {
    let (source_in, unseeked_prefix) = source_times;
    let trim = (unseeked_prefix
        - info
            .audio()
            .map(|audio| audio.start_time.unwrap_or(info.start_time) - info.start_time)
            .unwrap_or(0.0))
    .max(0.0);
    let lead = info
        .audio()
        .map(|audio| {
            (audio.start_time.unwrap_or(info.start_time) - info.start_time - source_in).max(0.0)
        })
        .unwrap_or(0.0);
    let content = (duration * speed - lead).max(0.0);
    if content <= 0.0 || volume == 0.0 {
        return format!(
            "anullsrc=r=48000:cl=stereo,atrim=duration={},asetpts=PTS-STARTPTS[{label}]",
            number(duration)
        );
    }
    // Retiming the content and padding the known stream delay separately prevents
    // WSOLA from moving the leading boundary when it stretches a run of silence.
    let delay = (lead / speed * 48000.0).round() as u64;
    let source_samples = (content * 48000.0).round() as u64;
    let output_samples = (duration * 48000.0).round() as u64;
    format!(
        "[{input}:a:0]asetpts=PTS-STARTPTS,atrim=start={},asetpts=PTS-STARTPTS,aresample=48000:async=1:first_pts=0,atrim=end_sample={source_samples},{},aformat=sample_fmts=fltp:channel_layouts=stereo,volume={},adelay={delay}S:all=1,apad=whole_len={output_samples},atrim=end_sample={output_samples},asetpts=N/SR/TB[{label}]",
        number(trim),
        tempo(speed),
        number(volume)
    )
}

fn render_plan(edit: &ValidatedEdit) -> Result<RenderPlan> {
    let mut args = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-filter_complex_threads".into(),
        "1".into(),
    ];
    let mut filters = Vec::new();
    let mut input = 0usize;
    let fps = edit.fps.ffmpeg();
    for (index, clip) in edit.clips.iter().enumerate() {
        let duration = edit.fps.seconds(clip.duration_frames);
        let seconds = number(duration);
        match clip.kind {
            MediaKind::Gap => {
                filters.push(format!(
                    "color=c=black:s={}x{}:r={}:d={},settb=AVTB[v{}]",
                    edit.width, edit.height, fps, seconds, index
                ));
                filters.push(format!("anullsrc=r=48000:cl=stereo,atrim=duration={seconds},asetpts=PTS-STARTPTS[a{index}]"));
            }
            MediaKind::Image | MediaKind::Video => {
                let source = clip
                    .source
                    .as_deref()
                    .context("validated source missing")?
                    .to_str()
                    .context("source path is not UTF-8")?;
                let unseeked_prefix = clip
                    .media
                    .as_ref()
                    .map(|info| unseeked_prefix(info, clip.source_in))
                    .unwrap_or(0.0);
                if clip.kind == MediaKind::Image {
                    push(
                        &mut args,
                        &[
                            "-loop",
                            "1",
                            "-framerate",
                            &fps,
                            "-t",
                            &seconds,
                            "-protocol_whitelist",
                            "file,pipe",
                            "-i",
                            source,
                        ],
                    );
                } else {
                    // Seeking to a video keyframe can skip audio before that
                    // frame, even with -ss 0. Decode a pre-video prefix from
                    // the beginning and trim its audio in the shared filter.
                    if clip.source_in > 0.0 && unseeked_prefix == 0.0 {
                        push(&mut args, &["-ss", &number(clip.source_in)]);
                    }
                    push(
                        &mut args,
                        &[
                            "-t",
                            &number(duration * clip.speed + unseeked_prefix),
                            "-protocol_whitelist",
                            "file,pipe",
                            "-i",
                            source,
                        ],
                    );
                }
                let video_trim = clip
                    .media
                    .as_ref()
                    .map(|info| {
                        (unseeked_prefix
                            - info
                                .video()
                                .and_then(|video| video.start_time)
                                .unwrap_or(info.start_time)
                            + info.start_time)
                            .max(0.0)
                    })
                    .unwrap_or(0.0);
                let video_lead = clip
                    .media
                    .as_ref()
                    .and_then(|info| {
                        info.video().map(|video| {
                            (video.start_time.unwrap_or(info.start_time)
                                - info.start_time
                                - clip.source_in)
                                .max(0.0)
                        })
                    })
                    .unwrap_or(0.0);
                if video_lead >= duration * clip.speed {
                    // An audio-only prefix has no video frame to seed tpad.
                    filters.push(format!(
                        "color=c=black:s={}x{}:r={}:d={},settb=AVTB[v{}]",
                        edit.width, edit.height, fps, seconds, index
                    ));
                } else {
                    // Rebase incoming PTS before adding the explicit prefix,
                    // or the source's video delay is counted twice.
                    filters.push(format!("[{input}:v:0]setpts=PTS-STARTPTS,trim=start={},setpts=PTS-STARTPTS,tpad=start_mode=add:start_duration={}:color=black,setpts=PTS/{},fps={},scale={}:{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1,format=yuv420p,tpad=stop_mode=clone:stop_duration=1,trim=duration={},settb=AVTB[v{}]",number(video_trim),number(video_lead),number(clip.speed),fps,edit.width,edit.height,edit.width,edit.height,seconds,index));
                }
                if clip.media.as_ref().is_some_and(media::MediaInfo::has_audio) && clip.volume > 0.0
                {
                    filters.push(audio_segment(
                        input,
                        clip.media.as_ref().context("validated media missing")?,
                        (clip.source_in, unseeked_prefix),
                        duration,
                        clip.speed,
                        clip.volume,
                        &format!("a{index}"),
                    ));
                } else {
                    filters.push(format!("anullsrc=r=48000:cl=stereo,atrim=duration={seconds},asetpts=PTS-STARTPTS[a{index}]"));
                }
                input += 1;
            }
        }
    }
    let mut concat = String::new();
    for index in 0..edit.clips.len() {
        write!(concat, "[v{index}][a{index}]")?;
    }
    write!(concat, "concat=n={}:v=1:a=1[vout][abase]", edit.clips.len())?;
    filters.push(concat);
    for (index, clip) in edit.audio.iter().enumerate() {
        let duration = edit.fps.seconds(clip.duration_frames);
        let prefix = unseeked_prefix(&clip.media, clip.source_in);
        if clip.source_in > 0.0 && prefix == 0.0 {
            push(&mut args, &["-ss", &number(clip.source_in)]);
        }
        push(
            &mut args,
            &[
                "-t",
                &number(duration * clip.speed + prefix),
                "-protocol_whitelist",
                "file,pipe",
                "-i",
                clip.source.to_str().context("source path is not UTF-8")?,
            ],
        );
        let delay = (edit.fps.seconds(clip.offset_frames) * 48000.0).round() as u64;
        filters.push(audio_segment(
            input,
            &clip.media,
            (clip.source_in, prefix),
            duration,
            clip.speed,
            clip.volume,
            &format!("extra_raw{index}"),
        ));
        filters.push(format!(
            "[extra_raw{index}]adelay={delay}S:all=1,asetpts=N/SR/TB[extra{index}]"
        ));
        input += 1;
    }
    if edit.audio.is_empty() {
        filters.push("[abase]anull[aout]".into());
    } else {
        let mut mix = "[abase]".to_owned();
        for index in 0..edit.audio.len() {
            write!(mix, "[extra{index}]")?;
        }
        write!(
            mix,
            "amix=inputs={}:duration=first:dropout_transition=0:normalize=0,alimiter=limit=0.98:level=0:latency=1[aout]",
            edit.audio.len() + 1
        )?;
        filters.push(mix);
    }
    let (sidecars, video) = caption_args(edit, &mut args, input, &mut filters, "vout")?;
    push(
        &mut args,
        &[
            "-filter_complex",
            &filters.join(";"),
            "-map",
            &format!("[{video}]"),
            "-map",
            "[aout]",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "18",
            "-pix_fmt",
            "yuv420p",
            "-threads",
            "2",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-r",
            &fps,
            "-t",
            &number(edit.duration()),
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
            "__SKYWRITE_OUTPUT__",
        ],
    );
    Ok(RenderPlan {
        args,
        sources: edit.protected_paths().map(Path::to_owned).collect(),
        sidecars,
        duration_seconds: edit.duration(),
        timeout_seconds: (edit.duration() * 20.0 + 120.0)
            .ceil()
            .clamp(120.0, 86_400.0) as u64,
    })
}

// Sidecar text is inert data. Only generated placeholders enter FFmpeg filters.
fn caption_args(
    edit: &ValidatedEdit,
    args: &mut Vec<String>,
    mut input: usize,
    filters: &mut Vec<String>,
    initial_video: &str,
) -> Result<(Vec<String>, String)> {
    let mut sidecars = Vec::new();
    let mut video = initial_video.to_owned();
    let mut soft = Vec::new();
    for track in &edit.captions {
        let index = sidecars.len();
        sidecars.push(captions::encode(&track.cues, "vtt")?);
        let marker = format!("__SKYWRITE_SIDECAR_{index}__");
        if track.mode == CaptionMode::Burn {
            let next = format!("vcaption{index}");
            filters.push(format!("[{video}]subtitles=filename={marker}[{next}]"));
            video = next;
        } else {
            push(
                args,
                &[
                    "-protocol_whitelist",
                    "file,pipe",
                    "-f",
                    "webvtt",
                    "-i",
                    &marker,
                ],
            );
            soft.push((input, track.language.clone()));
            input += 1;
        }
    }
    if !edit.chapters.is_empty() {
        let index = sidecars.len();
        sidecars.push(captions::chapter_metadata(&edit.chapters, edit.duration()));
        push(
            args,
            &[
                "-protocol_whitelist",
                "file,pipe",
                "-f",
                "ffmetadata",
                "-i",
                &format!("__SKYWRITE_SIDECAR_{index}__"),
                "-map_metadata",
                "-1",
                "-map_chapters",
                &input.to_string(),
            ],
        );
        for (index, chapter) in edit.chapters.iter().enumerate() {
            push(
                args,
                &[
                    &format!("-metadata:c:{index}"),
                    &format!("title={}", chapter.title),
                ],
            );
        }
    } else {
        push(args, &["-map_chapters", "-1"]);
    }
    for (index, (input, language)) in soft.iter().enumerate() {
        push(
            args,
            &[
                "-map",
                &format!("{input}:s:0"),
                &format!("-metadata:s:s:{index}"),
                &format!("language={language}"),
            ],
        );
    }
    if !soft.is_empty() {
        push(args, &["-c:s", "mov_text"]);
    }
    Ok((sidecars, video))
}
fn private_sidecars(plan: &RenderPlan) -> Result<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix("skywrite-captions-")
        .tempdir()?;
    for (index, text) in plan.sidecars.iter().enumerate() {
        std::fs::write(dir.path().join(format!("caption-{index}.vtt")), text)?;
    }
    Ok(dir)
}
fn plan_args(plan: &RenderPlan, dir: &Path) -> Result<Vec<OsString>> {
    let mut args = plan.args.clone();
    for index in 0..plan.sidecars.len() {
        let path = dir.join(format!("caption-{index}.vtt"));
        let path = path
            .to_str()
            .context("caption temporary path must be UTF-8")?;
        ensure!(
            path.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c)),
            "caption temporary path must contain only ASCII letters, digits, /._-"
        );
        let marker = format!("__SKYWRITE_SIDECAR_{index}__");
        for index in 0..args.len() {
            if args[index] == marker || (index > 0 && args[index - 1] == "-filter_complex") {
                args[index] = args[index].replace(&marker, path);
            }
        }
    }
    Ok(args.into_iter().map(OsString::from).collect())
}
/// Finish the output of an optional renderer with the same validated metadata.
pub fn mux_captions(
    input: &Path,
    edit: &ValidatedEdit,
    output: &Path,
    overwrite: bool,
) -> Result<Value> {
    prevent_source_overwrite(edit, output)?;
    io::protect_inputs(output, &[input.to_owned()])?;
    ensure!(
        overwrite || !output.exists(),
        "output already exists; pass --overwrite"
    );
    let input = media::canonical_file(input)?;
    let info = media::probe_for_timing(&input)?;
    ensure!(
        info.has_video() && (info.duration - edit.duration()).abs() < 0.1 + 1.0 / edit.fps.value(),
        "mux input must be a video matching the validated output timeline duration"
    );
    let mut args = vec![
        "-hide_banner".into(),
        "-nostdin".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-filter_complex_threads".into(),
        "1".into(),
    ];
    push(
        &mut args,
        &[
            "-protocol_whitelist",
            "file,pipe",
            "-i",
            input.to_str().context("input path must be UTF-8")?,
        ],
    );
    let mut filters = Vec::new();
    let (sidecars, video) = caption_args(edit, &mut args, 1, &mut filters, "0:v:0")?;
    if filters.is_empty() {
        push(&mut args, &["-map", "0:v:0", "-c:v", "copy"]);
    } else {
        push(
            &mut args,
            &[
                "-filter_complex",
                &filters.join(";"),
                "-map",
                &format!("[{video}]"),
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "18",
                "-pix_fmt",
                "yuv420p",
                "-threads",
                "2",
            ],
        );
    }
    push(
        &mut args,
        &[
            "-map",
            "0:a?",
            "-c:a",
            "copy",
            "-t",
            &number(edit.duration()),
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
            "__SKYWRITE_OUTPUT__",
        ],
    );
    let plan = RenderPlan {
        args,
        sidecars,
        sources: vec![input],
        duration_seconds: edit.duration(),
        timeout_seconds: (edit.duration() * 20.0 + 120.0)
            .ceil()
            .clamp(120.0, 86400.0) as u64,
    };
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temp = tempfile::Builder::new()
        .prefix(".skywrite-mux-")
        .suffix(".mp4")
        .tempfile_in(parent)?;
    let sidecars = private_sidecars(&plan)?;
    let mut args = plan_args(&plan, sidecars.path())?;
    *args.last_mut().context("missing output")? = temp.path().as_os_str().to_owned();
    process::run(
        "ffmpeg",
        &args,
        Duration::from_secs(plan.timeout_seconds),
        4 * 1024 * 1024,
    )?;
    let rendered = media::probe(temp.path())?;
    ensure!(
        rendered.has_video()
            && (rendered.duration - edit.duration()).abs() < 0.1 + 1.0 / edit.fps.value(),
        "mux output duration differs from timeline"
    );
    if overwrite {
        temp.persist(output).map_err(|e| e.error)?;
    } else {
        temp.persist_noclobber(output).map_err(|e| e.error)?;
    }
    Ok(
        json!({"output":output,"container":"mp4","caption_track_count":edit.captions.len(),"chapter_count":edit.chapters.len(),"duration_seconds":rendered.duration}),
    )
}

pub fn render(edit: &ValidatedEdit, output: &Path, overwrite: bool) -> Result<Value> {
    prevent_source_overwrite(edit, output)?;
    ensure!(
        overwrite || !output.exists(),
        "output already exists; pass --overwrite to replace it"
    );
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure!(
        parent.is_dir(),
        "output parent directory does not exist: {}",
        parent.display()
    );
    let temp = tempfile::Builder::new()
        .prefix(".skywrite-render-")
        .suffix(".mp4")
        .tempfile_in(parent)?;
    let plan = render_plan(edit)?;
    let sidecars = private_sidecars(&plan)?;
    let mut args = plan_args(&plan, sidecars.path())?;
    *args.last_mut().context("missing output argument")? = temp.path().as_os_str().to_owned();
    process::run(
        "ffmpeg",
        &args,
        Duration::from_secs(plan.timeout_seconds),
        4 * 1024 * 1024,
    )?;
    let rendered = media::probe(temp.path())?;
    ensure!(
        rendered.has_video() && rendered.has_audio(),
        "rendered output is missing audio or video"
    );
    ensure!(
        (rendered.duration - edit.duration()).abs() < 0.1 + 1.0 / edit.fps.value(),
        "rendered duration differs from requested timeline"
    );
    if overwrite {
        temp.persist(output).map_err(|e| e.error)?;
    } else {
        temp.persist_noclobber(output).map_err(|e| e.error)?;
    }
    Ok(
        json!({"output":output,"container":"mp4","duration_seconds":rendered.duration,"duration_frames":edit.duration_frames,"warnings":edit.warnings}),
    )
}

fn python_script(edit: &ValidatedEdit) -> Result<String> {
    let literal = serde_json::to_string(&serde_json::to_string(&render_plan(edit)?)?)?;
    Ok(format!(
        r##"#!/usr/bin/env python3
"""Skywrite edit: Python standard library and FFmpeg only. Outputs H.264/AAC MP4.
Run: python3 render.py --output edit.mp4 [--overwrite]
"""
import argparse
import json
import os
import signal
import time
from pathlib import Path
import subprocess
import sys
import tempfile

PLAN = json.loads({literal})

CANCELLED = None

def cancel(signum, frame):
    # Recording covers spawn assignment and private output publication without
    # raising in either critical section. The owner observes it before publish.
    global CANCELLED
    CANCELLED = signum

def run_ffmpeg(args):
    child = None
    group_owner = os.name == 'posix'
    if group_owner and not all(hasattr(os, name) for name in ('waitid', 'WNOWAIT', 'WEXITED', 'WNOHANG', 'P_PID')):
        raise RuntimeError('safe process-group ownership requires waitid(WNOWAIT) on this platform')
    try:
        child = subprocess.Popen(['ffmpeg', *args], stdin=subprocess.DEVNULL,
                                 start_new_session=group_owner)
        deadline = time.monotonic() + PLAN['timeout_seconds']
        while True:
            if CANCELLED is not None:
                raise SystemExit(128 + CANCELLED)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired(child.args, PLAN['timeout_seconds'])
            if group_owner:
                # Observe without reaping: the leader PID reserves the group
                # identity until every group signal in finally has completed.
                observed = os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
                code = None if observed is None else (observed.si_status if observed.si_code == os.CLD_EXITED else -observed.si_status)
            else:
                # Windows Popen retains a process handle, not a recycled PGID.
                code = child.poll()
            if code is not None:
                if CANCELLED is not None:
                    raise SystemExit(128 + CANCELLED)
                if code:
                    raise subprocess.CalledProcessError(code, child.args)
                return
            time.sleep(min(0.01, remaining))
    finally:
        if child is not None:
            # Descendants may still hold the output open after the leader exits.
            # Complete group cleanup before unlinking/publishing private output.
            def terminate(sig):
                try:
                    if group_owner:
                        os.killpg(child.pid, sig)
                    elif child.poll() is None:
                        child.kill() if sig == getattr(signal, 'SIGKILL', None) else child.terminate()
                except ProcessLookupError:
                    pass
            terminate(signal.SIGTERM)
            if group_owner:
                # Do not call poll/wait before the final group signal: either
                # one can release the leader identity even after normal exit.
                time.sleep(0.05)
            else:
                try:
                    child.wait(timeout=0.5)
                except subprocess.TimeoutExpired:
                    pass
            terminate(getattr(signal, 'SIGKILL', signal.SIGTERM))
            child.wait()

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True)
    parser.add_argument('--overwrite', action='store_true')
    options = parser.parse_args()
    output = Path(options.output).absolute()
    protected = [Path(__file__).resolve()] + [Path(source).resolve() for source in PLAN['sources']]
    if output.resolve() in protected:
        raise ValueError('output must not replace an input renderer script or original source media')
    if output.exists() and not options.overwrite:
        raise FileExistsError('output exists; pass --overwrite')
    handlers = {{sig: signal.signal(sig, cancel) for sig in (signal.SIGINT, signal.SIGTERM)}}
    fd, temporary = tempfile.mkstemp(prefix='.skywrite-render-', suffix='.mp4', dir=output.parent)
    os.close(fd)
    sidecars = tempfile.TemporaryDirectory(prefix='skywrite-captions-')
    try:
        args = PLAN['args'].copy()
        for index, content in enumerate(PLAN['sidecars']):
            path = Path(sidecars.name) / ('caption-%d.vtt' % index)
            path.write_text(content, encoding='utf-8')
            # Only generated ASCII paths enter the subtitle filter.
            if not all(c.isascii() and (c.isalnum() or c in '/._-') for c in str(path)):
                raise ValueError('caption temporary path must contain only ASCII letters, digits, /._-')
            marker = '__SKYWRITE_SIDECAR_%d__' % index
            args = [arg.replace(marker, str(path)) if arg == marker or (position > 0 and args[position-1] == '-filter_complex') else arg for position, arg in enumerate(args)]
        args[-1] = temporary
        run_ffmpeg(args)
        if CANCELLED is not None:
            raise SystemExit(128 + CANCELLED)
        if options.overwrite:
            os.replace(temporary, output)
        else:
            os.link(temporary, output)
            os.unlink(temporary)
        print(json.dumps({{'output': str(output), 'duration_seconds': PLAN['duration_seconds']}}))
    finally:
        sidecars.cleanup()
        if os.path.exists(temporary):
            os.unlink(temporary)
        for sig, handler in handlers.items():
            signal.signal(sig, handler)

if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(json.dumps({{'error': str(error)}}), file=sys.stderr)
        sys.exit(1)
"##
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escaping() {
        assert_eq!(xml("a'\"<&>"), "a&apos;&quot;&lt;&amp;&gt;");
    }
    #[test]
    fn atempo_covers_full_allowed_range() {
        assert_eq!(tempo(8.0), "atempo=2,atempo=2,atempo=2.000000000");
        assert_eq!(tempo(0.125), "atempo=0.5,atempo=0.5,atempo=0.500000000");
    }
}
