//! Deterministic output backends compiled from a validated edit.
use crate::{
    captions::{self, CaptionMode},
    io, media, process,
    timeline::{MediaKind, ValidatedEdit, ValidatedMix, ValidatedVideoClip},
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
mod xmeml;

pub fn export(edit: &ValidatedEdit, target: &str, output: &Path, overwrite: bool) -> Result<Value> {
    prevent_source_overwrite(edit, output)?;
    let mut warnings = edit.warnings.clone();
    let mut sidecar_files: Vec<PathBuf> = Vec::new();
    match target {
        "fcpxml" => {
            io::atomic_write(output, fcpxml::compile(edit)?.as_bytes(), overwrite)?;
            warnings.extend(interchange_warnings(edit, "FCPXML"));
            if !edit.captions.is_empty() {
                warnings.push("Caption tracks are embedded as native FCPXML captions (SRT role) above the video tracks. Burn-in is not applied. Final Cut/Resolve import of embedded captions has not been verified; please report results with skywrite nit. Unknown languages are labelled English.".into());
            }
        }
        "premiere" | "xmeml" | "fcp7" => {
            let (xml, notes) = xmeml::compile(edit)?;
            io::atomic_write(output, xml.as_bytes(), overwrite)?;
            warnings.extend(interchange_warnings(edit, "Premiere XML"));
            warnings.extend(notes);
            if !edit.captions.is_empty() {
                let files = caption_sidecars(edit, output, overwrite)?;
                warnings.push(format!(
                    "Premiere XML cannot carry captions, so each caption track was written beside it as SRT ({}). In Premiere, import the XML and the SRT together (File > Import), then drag the caption item onto the sequence at 00:00:00:00; the captions stay editable as text.",
                    files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join(", ")
                ));
                sidecar_files = files;
            }
        }
        "ffmpeg" | "python" => {
            io::atomic_write(output, python_script(edit)?.as_bytes(), overwrite)?
        }
        "hyperframes" | "remotion" => return project::export(edit, target, output),
        _ => bail!(
            "unsupported export target {target}; choose fcpxml, premiere, ffmpeg, hyperframes, or remotion"
        ),
    }
    Ok(
        json!({"target":target,"output":output,"caption_files":sidecar_files,"duration_seconds":edit.duration(),"duration_frames":edit.duration_frames,"warnings":warnings}),
    )
}

/// Write one validated caption track (output timeline) as SRT or VTT.
pub fn write_caption_track(
    edit: &ValidatedEdit,
    index: usize,
    output: &Path,
    format: Option<&str>,
    overwrite: bool,
) -> Result<Value> {
    prevent_source_overwrite(edit, output)?;
    let track = edit.captions.get(index).with_context(|| {
        format!(
            "edit has {} caption tracks; add one (for example {{\"from_transcripts\": [\"words.json\"]}}) or choose --track",
            edit.captions.len()
        )
    })?;
    let format = match format.or_else(|| output.extension().and_then(|e| e.to_str())) {
        Some("vtt") => "vtt",
        Some("srt") | None => "srt",
        Some(other) => bail!("caption format must be srt or vtt, not {other}"),
    };
    io::atomic_write(
        output,
        captions::encode(&track.cues, format)?.as_bytes(),
        overwrite,
    )?;
    Ok(
        json!({"output":output,"format":format,"track":index,"language":track.language,"cue_count":track.cues.len(),
        "timeline":"output","note":"Cues are on the edit's output timeline. Import alongside the timeline export so captions stay editable in the NLE."}),
    )
}

/// Sidecar caption files written next to an NLE export: NAME.LANG[.N].srt.
fn caption_sidecars(edit: &ValidatedEdit, output: &Path, overwrite: bool) -> Result<Vec<PathBuf>> {
    let stem = output
        .file_stem()
        .and_then(|s| s.to_str())
        .context("export output needs a file name")?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut written = Vec::new();
    for (index, track) in edit.captions.iter().enumerate() {
        let repeated = edit
            .captions
            .iter()
            .filter(|t| t.language == track.language)
            .count()
            > 1;
        let name = if repeated {
            format!("{stem}.{}.{index}.srt", track.language)
        } else {
            format!("{stem}.{}.srt", track.language)
        };
        let path = parent.join(name);
        prevent_source_overwrite(edit, &path)?;
        io::atomic_write(
            &path,
            captions::encode(&track.cues, "srt")?.as_bytes(),
            overwrite,
        )?;
        written.push(path);
    }
    Ok(written)
}

/// Finishing that NLE interchange cannot carry identically to the MP4 render.
fn interchange_warnings(edit: &ValidatedEdit, format: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    for source in &edit.sources {
        if let Some(lut) = &source.lut {
            warnings.push(format!(
                "{format} cannot reference LUT {} for {}; apply it in the NLE (Lumetri/Custom LUT) to match the rendered MP4",
                lut.display(),
                source.path.display()
            ));
        }
    }
    if let Some(limit) = edit.master.limiter_db {
        warnings.push(format!(
            "{format} has no master limiter; add a {limit} dBFS limiter on the NLE mix bus to match the rendered MP4"
        ));
    }
    warnings
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
    sidecars: Vec<Sidecar>,
}
/// Generated private file content. Names are fixed ASCII; text is inert data.
#[derive(Debug, Serialize)]
struct Sidecar {
    name: String,
    text: String,
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

/// Linear gain curve as an FFmpeg expression of segment-relative time `t`.
fn envelope_expression(mix: &ValidatedMix) -> String {
    let k = &mix.keyframes;
    let mut expr = number(k[k.len() - 1].volume);
    for i in (0..k.len() - 1).rev() {
        let (a, b) = (k[i], k[i + 1]);
        let span = b.time - a.time;
        expr = format!(
            "if(lt(t,{}),{}+({})*(t-{})/{},{expr})",
            number(b.time),
            number(a.volume),
            number(b.volume - a.volume),
            number(a.time),
            number(span)
        );
    }
    format!(
        "{}*if(lt(t,{}),{},{expr})",
        number(mix.volume),
        number(k[0].time),
        number(k[0].volume)
    )
}

/// Gain, keyframe envelope, then linear fades, applied in output time.
fn mix_filters(mix: &ValidatedMix, duration: f64) -> String {
    let mut parts = Vec::new();
    if mix.keyframes.is_empty() {
        if mix.volume != 1.0 {
            parts.push(format!("volume={}", number(mix.volume)));
        }
    } else {
        // Commas are protected by filtergraph quoting; no user text is present.
        parts.push(format!(
            "volume=eval=frame:volume='{}'",
            envelope_expression(mix)
        ));
    }
    if mix.fade_in > 0.0 {
        parts.push(format!(
            "afade=t=in:st=0:d={}:curve=tri",
            number(mix.fade_in)
        ));
    }
    if mix.fade_out > 0.0 {
        parts.push(format!(
            "afade=t=out:st={}:d={}:curve=tri",
            number(duration - mix.fade_out),
            number(mix.fade_out)
        ));
    }
    if parts.is_empty() {
        "anull".into()
    } else {
        parts.join(",")
    }
}

fn silence(samples: u64, label: &str) -> String {
    format!("anullsrc=r=48000:cl=stereo,atrim=end_sample={samples},asetpts=PTS-STARTPTS[{label}]")
}

fn audio_segment(
    input: usize,
    info: &media::MediaInfo,
    source_times: (f64, f64),
    output_samples: u64,
    speed: f64,
    envelope: &str,
    label: &str,
) -> String {
    let (source_in, unseeked_prefix) = source_times;
    let duration = output_samples as f64 / 48000.0;
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
    if content <= 0.0 {
        return silence(output_samples, label);
    }
    // Retiming the content and padding the known stream delay separately prevents
    // WSOLA from moving the leading boundary when it stretches a run of silence.
    let delay = (lead / speed * 48000.0).round() as u64;
    let source_samples = (content * 48000.0).round() as u64;
    format!(
        "[{input}:a:0]asetpts=PTS-STARTPTS,atrim=start={},asetpts=PTS-STARTPTS,aresample=48000:async=1:first_pts=0,atrim=end_sample={source_samples},{},aformat=sample_fmts=fltp:channel_layouts=stereo,adelay={delay}S:all=1,apad=whole_len={output_samples},atrim=end_sample={output_samples},asetpts=N/SR/TB,{envelope}[{label}]",
        number(trim),
        tempo(speed),
    )
}

fn transparent(edit: &ValidatedEdit, frames: u64, label: &str) -> String {
    format!(
        "color=c=black@0:s={}x{}:r={},format=yuva420p,trim=end_frame={frames},setpts=PTS-STARTPTS,settb=AVTB[{label}]",
        edit.width,
        edit.height,
        edit.fps.ffmpeg()
    )
}

/// Add one clip's input and return its picture chain.
fn video_clip(
    edit: &ValidatedEdit,
    clip: &ValidatedVideoClip,
    args: &mut Vec<String>,
    input: usize,
    lut_marker: Option<(&str, bool)>,
    label: &str,
) -> Result<(String, f64)> {
    let fps = edit.fps.ffmpeg();
    let duration = edit.fps.seconds(clip.duration_frames);
    let frames = clip.duration_frames;
    let source = clip.source.to_str().context("source path is not UTF-8")?;
    let info = &clip.media;
    let prefix = if clip.kind == MediaKind::Image {
        0.0
    } else {
        unseeked_prefix(info, clip.source_in)
    };
    if clip.kind == MediaKind::Image {
        push(
            args,
            &[
                "-loop",
                "1",
                "-framerate",
                &fps,
                "-t",
                &number(duration),
                "-protocol_whitelist",
                "file,pipe",
                "-i",
                source,
            ],
        );
    } else {
        // Seeking to a video keyframe can skip audio before that frame, even
        // with -ss 0. Decode a pre-video prefix from the beginning and trim
        // its audio in the shared filter.
        if clip.source_in > 0.0 && prefix == 0.0 {
            push(args, &["-ss", &number(clip.source_in)]);
        }
        push(
            args,
            &[
                "-t",
                &number(duration * clip.speed + prefix),
                "-protocol_whitelist",
                "file,pipe",
                "-i",
                source,
            ],
        );
    }
    let video_trim = (prefix
        - info
            .video()
            .and_then(|v| v.start_time)
            .unwrap_or(info.start_time)
        + info.start_time)
        .max(0.0);
    let video_lead = info
        .video()
        .map(|video| {
            (video.start_time.unwrap_or(info.start_time) - info.start_time - clip.source_in)
                .max(0.0)
        })
        .unwrap_or(0.0);
    if clip.kind == MediaKind::Video && video_lead >= duration * clip.speed {
        // An audio-only prefix has no video frame to seed tpad.
        return Ok((transparent(edit, frames, label), prefix));
    }
    let mut chain = format!(
        "[{input}:v:0]setpts=PTS-STARTPTS,trim=start={},setpts=PTS-STARTPTS,tpad=start_mode=add:start_duration={}:color=black,setpts=PTS/{},fps={fps}",
        number(video_trim),
        number(video_lead),
        number(clip.speed)
    );
    if let Some((marker, three_d)) = lut_marker {
        // Only generated ASCII sidecar paths enter the filter graph.
        let filter = if three_d { "lut3d" } else { "lut1d" };
        write!(chain, ",{filter}=file={marker}:interp=tetrahedral")?;
        if !three_d {
            chain = chain.replace(":interp=tetrahedral", ":interp=linear");
        }
    }
    write!(
        chain,
        ",scale={w}:{h}:force_original_aspect_ratio=decrease,format=yuva420p,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black@0,setsar=1,tpad=stop_mode=clone:stop_duration=1,trim=end_frame={frames}",
        w = edit.width,
        h = edit.height
    )?;
    if clip.opacity < 1.0 {
        write!(chain, ",colorchannelmixer=aa={}", number(clip.opacity))?;
    }
    if clip.fade_in > 0.0 {
        write!(chain, ",fade=t=in:st=0:d={}:alpha=1", number(clip.fade_in))?;
    }
    if clip.fade_out > 0.0 {
        write!(
            chain,
            ",fade=t=out:st={}:d={}:alpha=1",
            number(duration - clip.fade_out),
            number(clip.fade_out)
        )?;
    }
    write!(chain, ",setpts=PTS-STARTPTS,settb=AVTB[{label}]")?;
    Ok((chain, prefix))
}

fn concat(labels: &[String], video: bool, out: &str) -> String {
    let mut text = labels.iter().map(|l| format!("[{l}]")).collect::<String>();
    if labels.len() == 1 {
        text.push_str(if video { "null" } else { "anull" });
    } else {
        write!(
            text,
            "concat=n={}:v={}:a={}",
            labels.len(),
            u8::from(video),
            u8::from(!video)
        )
        .expect("string write");
    }
    write!(text, "[{out}]").expect("string write");
    text
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
    let mut sidecars = Vec::new();
    let lut_markers = edit
        .luts
        .iter()
        .map(|lut| {
            let index = sidecars.len();
            sidecars.push(Sidecar {
                name: format!("lut-{index}.cube"),
                text: lut.text.clone(),
            });
            (
                format!("__SKYWRITE_SIDECAR_{index}__"),
                lut.text
                    .lines()
                    .any(|l| l.trim_start().starts_with("LUT_3D_SIZE")),
            )
        })
        .collect::<Vec<_>>();
    let mut filters = Vec::new();
    let mut input = 0usize;
    let total = edit.duration_frames;
    let total_samples = edit.fps.samples_48k(total);
    filters.push(format!(
        "color=c=black:s={}x{}:r={},format=yuv420p,trim=end_frame={total},setpts=PTS-STARTPTS,settb=AVTB[base]",
        edit.width,
        edit.height,
        edit.fps.ffmpeg()
    ));
    let mut current = "base".to_owned();
    let mut lanes = Vec::<String>::new();
    for (t, track) in edit.video.iter().enumerate() {
        if track.clips.is_empty() {
            continue;
        }
        let mut pictures = Vec::new();
        let mut sounds = Vec::new();
        let mut sound_filters = Vec::new();
        let mut cursor = 0u64;
        let mut has_sound = false;
        for (c, clip) in track.clips.iter().enumerate() {
            if clip.start_frames > cursor {
                let label = format!("vg{t}_{c}");
                filters.push(transparent(edit, clip.start_frames - cursor, &label));
                pictures.push(label);
                let label = format!("ag{t}_{c}");
                sound_filters.push(silence(
                    edit.fps.samples_48k(clip.start_frames) - edit.fps.samples_48k(cursor),
                    &label,
                ));
                sounds.push(label);
            }
            let label = format!("v{t}_{c}");
            let marker = clip
                .lut
                .map(|i| (lut_markers[i].0.as_str(), lut_markers[i].1));
            let (chain, prefix) = video_clip(edit, clip, &mut args, input, marker, &label)?;
            filters.push(chain);
            pictures.push(label);
            let samples =
                edit.fps.samples_48k(clip.end_frames()) - edit.fps.samples_48k(clip.start_frames);
            let label = format!("a{t}_{c}");
            if clip.has_audible_sound() {
                has_sound = true;
                sound_filters.push(audio_segment(
                    input,
                    &clip.media,
                    (clip.source_in, prefix),
                    samples,
                    clip.speed,
                    &mix_filters(&clip.audio, edit.fps.seconds(clip.duration_frames)),
                    &label,
                ));
            } else {
                sound_filters.push(silence(samples, &label));
            }
            sounds.push(label);
            cursor = clip.end_frames();
            input += 1;
        }
        filters.push(concat(&pictures, true, &format!("track{t}")));
        let next = format!("comp{t}");
        filters.push(format!(
            "[{current}][track{t}]overlay=x=0:y=0:eof_action=pass:repeatlast=0:format=yuv420[{next}]"
        ));
        current = next;
        if has_sound {
            filters.extend(sound_filters);
            let lane = format!("lane_v{t}");
            filters.push(concat(&sounds, false, &lane));
            lanes.push(lane);
        }
    }
    filters.push(format!("[{current}]format=yuv420p,setsar=1[vout]"));
    for (t, track) in edit.audio.iter().enumerate() {
        let mut sounds = Vec::new();
        let mut cursor = 0u64;
        for (c, clip) in track.clips.iter().filter(|c| c.mix.audible()).enumerate() {
            if clip.start_frames > cursor {
                let label = format!("xg{t}_{c}");
                filters.push(silence(
                    edit.fps.samples_48k(clip.start_frames) - edit.fps.samples_48k(cursor),
                    &label,
                ));
                sounds.push(label);
            }
            let prefix = unseeked_prefix(&clip.media, clip.source_in);
            let duration = edit.fps.seconds(clip.duration_frames);
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
            let label = format!("x{t}_{c}");
            filters.push(audio_segment(
                input,
                &clip.media,
                (clip.source_in, prefix),
                edit.fps.samples_48k(clip.end_frames()) - edit.fps.samples_48k(clip.start_frames),
                clip.speed,
                &mix_filters(&clip.mix, duration),
                &label,
            ));
            sounds.push(label);
            cursor = clip.end_frames();
            input += 1;
        }
        if !sounds.is_empty() {
            let lane = format!("lane_a{t}");
            filters.push(concat(&sounds, false, &lane));
            lanes.push(lane);
        }
    }
    let mut mix = match lanes.len() {
        0 => {
            filters.push(silence(total_samples, "mixed"));
            "[mixed]".to_owned()
        }
        1 => format!("[{}]", lanes[0]),
        n => {
            let mut text = lanes.iter().map(|l| format!("[{l}]")).collect::<String>();
            write!(
                text,
                "amix=inputs={n}:duration=longest:dropout_transition=0:normalize=0"
            )?;
            text
        }
    };
    if lanes.len() <= 1 {
        mix.push_str("anull");
    }
    if let Some(limit) = edit.master.limiter_db {
        write!(
            mix,
            ",alimiter=limit={}:level=0:latency=1",
            number(10f64.powf(limit / 20.0).max(0.0625))
        )?;
    }
    write!(
        mix,
        ",apad=whole_len={total_samples},atrim=end_sample={total_samples},asetpts=N/SR/TB[aout]"
    )?;
    filters.push(mix);
    let video = caption_args(edit, &mut args, input, &mut filters, "vout", &mut sidecars)?;
    // Frames are composited upright on the output canvas. Some FFmpeg builds
    // still carry a source's display matrix (iPhone rotation) to the encoder,
    // which makes players turn the finished picture sideways; drop it.
    filters.push(format!(
        "[{video}]sidedata=mode=delete:type=DISPLAYMATRIX[vfinal]"
    ));
    let fps = edit.fps.ffmpeg();
    push(
        &mut args,
        &[
            "-filter_complex",
            &filters.join(";"),
            "-map",
            "[vfinal]",
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
    sidecars: &mut Vec<Sidecar>,
) -> Result<String> {
    let mut video = initial_video.to_owned();
    let mut soft = Vec::new();
    for track in &edit.captions {
        let index = sidecars.len();
        sidecars.push(Sidecar {
            name: format!("caption-{index}.vtt"),
            text: captions::encode(&track.cues, "vtt")?,
        });
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
        sidecars.push(Sidecar {
            name: format!("chapters-{index}.txt"),
            text: captions::chapter_metadata(&edit.chapters, edit.duration()),
        });
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
    Ok(video)
}
fn private_sidecars(plan: &RenderPlan) -> Result<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix("skywrite-sidecars-")
        .tempdir()?;
    for sidecar in &plan.sidecars {
        std::fs::write(dir.path().join(&sidecar.name), &sidecar.text)?;
    }
    Ok(dir)
}
fn plan_args(plan: &RenderPlan, dir: &Path) -> Result<Vec<OsString>> {
    let mut args = plan.args.clone();
    for (index, sidecar) in plan.sidecars.iter().enumerate() {
        let path = dir.join(&sidecar.name);
        let path = path
            .to_str()
            .context("sidecar temporary path must be UTF-8")?;
        ensure!(
            path.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c)),
            "sidecar temporary path must contain only ASCII letters, digits, /._-"
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
    let mut sidecars = Vec::new();
    let video = caption_args(edit, &mut args, 1, &mut filters, "0:v:0", &mut sidecars)?;
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
        crate::io::persist_new(temp, output)?;
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
    let mut warnings = edit.warnings.clone();
    let temp = match display_rotation(temp.path())? {
        Some(rotation) => {
            let cleared = clear_display_rotation(temp.path(), parent)?;
            if let Some(left) = display_rotation(cleared.path())? {
                warnings.push(format!(
                    "the output still carries a {left} degree display rotation that this FFmpeg could not remove; players may show it sideways"
                ));
            } else {
                warnings.push(format!(
                    "removed a {rotation} degree display rotation that FFmpeg copied from a source; the picture is already upright"
                ));
            }
            cleared
        }
        None => temp,
    };
    if overwrite {
        temp.persist(output).map_err(|e| e.error)?;
    } else {
        crate::io::persist_new(temp, output)?;
    }
    let peak = peak_dbfs(output)?;
    if let Some(peak) = peak
        && peak >= -0.1
    {
        warnings.push(format!(
            "the mix peaks at {peak:.2} dBFS and is likely clipping; lower gains or set master.limiter_db (for example -1)"
        ));
    }
    Ok(
        json!({"output":output,"container":"mp4","duration_seconds":rendered.duration,"duration_frames":edit.duration_frames,"peak_dbfs":peak,"warnings":warnings}),
    )
}

/// Display-matrix rotation of the first video stream, when it is not zero.
fn display_rotation(path: &Path) -> Result<Option<String>> {
    let mut args = process::args(&[
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream_side_data=rotation",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
    ]);
    args.push(path.as_os_str().to_owned());
    let out = process::run("ffprobe", &args, Duration::from_secs(60), 64 * 1024)?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|value| value.parse::<f64>().is_ok_and(|v| v != 0.0))
        .map(str::to_owned))
}

/// Stream-copy `path` with an identity display matrix (FFmpeg 7+), falling
/// back to the legacy `rotate` tag for older builds.
fn clear_display_rotation(path: &Path, parent: &Path) -> Result<tempfile::NamedTempFile> {
    let cleared = tempfile::Builder::new()
        .prefix(".skywrite-render-")
        .suffix(".mp4")
        .tempfile_in(parent)?;
    let attempt = |modern: bool| -> Result<()> {
        let mut args = process::args(&["-hide_banner", "-nostdin", "-v", "error", "-y"]);
        if modern {
            args.extend(process::args(&["-display_rotation", "0"]));
        }
        args.extend(process::args(&["-i"]));
        args.push(path.as_os_str().to_owned());
        args.extend(process::args(&[
            "-map",
            "0",
            "-c",
            "copy",
            "-map_chapters",
            "0",
        ]));
        if !modern {
            args.extend(process::args(&["-metadata:s:v:0", "rotate=0"]));
        }
        args.extend(process::args(&["-movflags", "+faststart", "-f", "mp4"]));
        args.push(cleared.path().as_os_str().to_owned());
        process::run("ffmpeg", &args, Duration::from_secs(600), 4 * 1024 * 1024).map(|_| ())
    };
    if attempt(true).is_err() {
        attempt(false)?;
    }
    Ok(cleared)
}

/// Sample peak of the decoded output audio in dBFS (None when silent or absent).
fn peak_dbfs(path: &Path) -> Result<Option<f64>> {
    let mut args = process::args(&["-hide_banner", "-nostdin", "-v", "info", "-i"]);
    args.push(path.as_os_str().to_owned());
    args.extend(process::args(&[
        "-map",
        "0:a:0?",
        "-af",
        "astats=measure_perchannel=none:measure_overall=Peak_level",
        "-f",
        "null",
        "-",
    ]));
    let out = process::run("ffmpeg", &args, Duration::from_secs(600), 4 * 1024 * 1024)?;
    let log = String::from_utf8_lossy(&out.stderr);
    Ok(log
        .lines()
        .rev()
        .filter_map(|line| line.split("Peak level dB:").nth(1))
        .filter_map(|value| value.trim().parse::<f64>().ok())
        .find(|value| value.is_finite()))
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
    sidecars = tempfile.TemporaryDirectory(prefix='skywrite-sidecars-')
    try:
        args = PLAN['args'].copy()
        for index, sidecar in enumerate(PLAN['sidecars']):
            path = Path(sidecars.name) / sidecar['name']
            path.write_text(sidecar['text'], encoding='utf-8')
            # Only generated ASCII paths enter caption and LUT filters.
            if not all(c.isascii() and (c.isalnum() or c in '/._-') for c in str(path)):
                raise ValueError('sidecar temporary path must contain only ASCII letters, digits, /._-')
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
    fn display_rotation_is_detected_and_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.mp4");
        let rotated = dir.path().join("rotated.mp4");
        let run = |args: &[&str]| {
            let status = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-y"])
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };
        run(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=64x36:r=10:d=0.5",
            "-c:v",
            "libx264",
            plain.to_str().unwrap(),
        ]);
        run(&[
            "-display_rotation",
            "-90",
            "-i",
            plain.to_str().unwrap(),
            "-c",
            "copy",
            rotated.to_str().unwrap(),
        ]);
        assert_eq!(display_rotation(&plain).unwrap(), None);
        assert!(display_rotation(&rotated).unwrap().is_some());
        let cleared = clear_display_rotation(&rotated, dir.path()).unwrap();
        assert_eq!(display_rotation(cleared.path()).unwrap(), None);
    }
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
