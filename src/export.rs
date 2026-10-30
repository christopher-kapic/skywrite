//! Deterministic output backends compiled from a validated edit.
use crate::{
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
    Ok(
        json!({"target":target,"output":output,"duration_seconds":edit.duration(),"duration_frames":edit.duration_frames,"warnings":edit.warnings}),
    )
}
fn prevent_source_overwrite(edit: &ValidatedEdit, path: &Path) -> Result<()> {
    if let Ok(path) = path.canonicalize() {
        ensure!(
            !edit.source_paths().any(|source| source == path),
            "output must not replace an original source media file"
        );
    }
    Ok(())
}
#[derive(Debug, Serialize)]
struct RenderPlan {
    args: Vec<String>,
    sources: Vec<PathBuf>,
    duration_seconds: f64,
    timeout_seconds: u64,
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

fn audio_segment(
    input: usize,
    info: &media::MediaInfo,
    source_in: f64,
    duration: f64,
    speed: f64,
    volume: f64,
    label: &str,
) -> String {
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
        "[{input}:a:0]asetpts=PTS-STARTPTS,aresample=48000:async=1:first_pts=0,atrim=end_sample={source_samples},{},aformat=sample_fmts=fltp:channel_layouts=stereo,volume={},adelay={delay}S:all=1,apad=whole_len={output_samples},atrim=end_sample={output_samples},asetpts=N/SR/TB[{label}]",
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
                    push(
                        &mut args,
                        &[
                            "-ss",
                            &number(clip.source_in),
                            "-t",
                            &number(duration * clip.speed),
                            "-protocol_whitelist",
                            "file,pipe",
                            "-i",
                            source,
                        ],
                    );
                }
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
                filters.push(format!("[{input}:v:0]tpad=start_mode=add:start_duration={}:color=black,setpts=(PTS-STARTPTS)/{},fps={},scale={}:{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1,format=yuv420p,tpad=stop_mode=clone:stop_duration=1,trim=duration={},settb=AVTB[v{}]",number(video_lead),number(clip.speed),fps,edit.width,edit.height,edit.width,edit.height,seconds,index));
                if clip.media.as_ref().is_some_and(media::MediaInfo::has_audio) && clip.volume > 0.0
                {
                    filters.push(audio_segment(
                        input,
                        clip.media.as_ref().context("validated media missing")?,
                        clip.source_in,
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
        push(
            &mut args,
            &[
                "-ss",
                &number(clip.source_in),
                "-t",
                &number(duration * clip.speed),
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
            clip.source_in,
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
    push(
        &mut args,
        &[
            "-filter_complex",
            &filters.join(";"),
            "-map",
            "[vout]",
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
        sources: edit.source_paths().map(Path::to_owned).collect(),
        duration_seconds: edit.duration(),
        timeout_seconds: (edit.duration() * 20.0 + 120.0)
            .ceil()
            .clamp(120.0, 86_400.0) as u64,
    })
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
    let mut args: Vec<OsString> = plan.args.iter().map(OsString::from).collect();
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
from pathlib import Path
import subprocess
import sys
import tempfile

PLAN = json.loads({literal})

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True)
    parser.add_argument('--overwrite', action='store_true')
    options = parser.parse_args()
    output = Path(options.output).absolute()
    if output.resolve() in [Path(source).resolve() for source in PLAN['sources']]:
        raise ValueError('output must not replace original source media')
    if output.exists() and not options.overwrite:
        raise FileExistsError('output exists; pass --overwrite')
    fd, temporary = tempfile.mkstemp(prefix='.skywrite-render-', suffix='.mp4', dir=output.parent)
    os.close(fd)
    try:
        args = PLAN['args'].copy()
        args[-1] = temporary
        subprocess.run(['ffmpeg', *args], check=True, timeout=PLAN['timeout_seconds'], stdin=subprocess.DEVNULL)
        if options.overwrite:
            os.replace(temporary, output)
        else:
            os.link(temporary, output)
            os.unlink(temporary)
        print(json.dumps({{'output': str(output), 'duration_seconds': PLAN['duration_seconds']}}))
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)

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
