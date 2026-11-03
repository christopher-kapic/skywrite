//! FCPXML 1.10, following Apple's FCPXML reference and timeMap specification.
use super::xml;
use crate::timeline::{self, MediaKind, Rational, ValidatedEdit, ValidatedMix, ValidatedVideoClip};
use anyhow::Result;
use std::{collections::HashMap, fmt::Write as _, path::PathBuf};

fn seconds(value: f64) -> String {
    if value < 0.0 {
        format!(
            "-{}",
            timeline::rational_time((-value * 1_000_000_000.0).round() as u64, 1_000_000_000)
        )
    } else {
        timeline::rational_time((value * 1_000_000_000.0).round() as u64, 1_000_000_000)
    }
}
fn parse_fps(value: &str) -> Option<Rational> {
    let (num, den) = value.split_once('/')?;
    let num = num.parse::<u32>().ok()?;
    let den = den.parse::<u32>().ok()?;
    (num > 0 && den > 0).then_some(Rational { num, den })
}
fn time_map(out: &mut String, start: f64, duration: f64, speed: f64) -> Result<()> {
    writeln!(
        out,
        "<timeMap preservesPitch=\"1\"><timept time=\"0s\" value=\"{}\" interp=\"linear\"/><timept time=\"{}\" value=\"{}\" interp=\"linear\"/></timeMap>",
        seconds(start),
        seconds(duration),
        seconds(start + duration * speed)
    )?;
    Ok(())
}

fn decibels(gain: f64) -> String {
    if gain <= 0.0 {
        "-96dB".into()
    } else {
        format!("{:.6}dB", (20.0 * gain.log10()).max(-96.0))
    }
}

/// Clip-local picture opacity and fades.
fn blend(
    out: &mut String,
    clip: &ValidatedVideoClip,
    local_start: f64,
    duration: f64,
) -> Result<()> {
    if clip.opacity == 1.0 && clip.fade_in == 0.0 && clip.fade_out == 0.0 {
        return Ok(());
    }
    if clip.fade_in == 0.0 && clip.fade_out == 0.0 {
        writeln!(out, "<adjust-blend amount=\"{:.6}\"/>", clip.opacity)?;
        return Ok(());
    }
    let mut points = vec![(
        0.0,
        if clip.fade_in > 0.0 {
            0.0
        } else {
            clip.opacity
        },
    )];
    if clip.fade_in > 0.0 {
        points.push((clip.fade_in, clip.opacity));
    }
    if clip.fade_out > 0.0 {
        points.push((duration - clip.fade_out, clip.opacity));
    }
    points.push((
        duration,
        if clip.fade_out > 0.0 {
            0.0
        } else {
            clip.opacity
        },
    ));
    points.dedup_by(|a, b| (a.0 - b.0).abs() < 1e-9);
    writeln!(
        out,
        "<adjust-blend amount=\"{:.6}\"><param name=\"amount\"><keyframeAnimation>",
        clip.opacity
    )?;
    for (time, value) in points {
        writeln!(
            out,
            "<keyframe time=\"{}\" value=\"{value:.6}\"/>",
            seconds(local_start + time)
        )?;
    }
    writeln!(out, "</keyframeAnimation></param></adjust-blend>")?;
    Ok(())
}

/// Clip-local gain, keyframes and fades.
fn volume(out: &mut String, mix: &ValidatedMix, local_start: f64) -> Result<()> {
    if mix.is_unity() {
        return Ok(());
    }
    let amount = if mix.keyframes.is_empty() {
        decibels(mix.volume)
    } else {
        "0dB".into()
    };
    if mix.keyframes.is_empty() && mix.fade_in == 0.0 && mix.fade_out == 0.0 {
        writeln!(out, "<adjust-volume amount=\"{amount}\"/>")?;
        return Ok(());
    }
    writeln!(
        out,
        "<adjust-volume amount=\"{amount}\"><param name=\"amount\">"
    )?;
    if mix.fade_in > 0.0 {
        writeln!(
            out,
            "<fadeIn type=\"linear\" duration=\"{}\"/>",
            seconds(mix.fade_in)
        )?;
    }
    if mix.fade_out > 0.0 {
        writeln!(
            out,
            "<fadeOut type=\"linear\" duration=\"{}\"/>",
            seconds(mix.fade_out)
        )?;
    }
    if !mix.keyframes.is_empty() {
        writeln!(out, "<keyframeAnimation>")?;
        for keyframe in &mix.keyframes {
            writeln!(
                out,
                "<keyframe time=\"{}\" value=\"{}\"/>",
                seconds(local_start + keyframe.time),
                decibels(mix.volume * keyframe.volume)
            )?;
        }
        writeln!(out, "</keyframeAnimation>")?;
    }
    writeln!(out, "</param></adjust-volume>")?;
    Ok(())
}

fn gap(out: &mut String, edit: &ValidatedEdit, from: u64, to: u64) -> Result<()> {
    if to > from {
        writeln!(
            out,
            "<gap name=\"Gap\" offset=\"{}\" start=\"0s\" duration=\"{}\"/>",
            edit.fps.fcpxml(from),
            edit.fps.fcpxml(to - from)
        )?;
    }
    Ok(())
}

fn file_name(path: &std::path::Path, fallback: &str) -> String {
    xml(path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or(fallback))
}

/// FCPXML caption roles use two-letter language codes.
fn two_letter(code: &str) -> &str {
    match code {
        "eng" => "en",
        "spa" => "es",
        "fra" | "fre" => "fr",
        "deu" | "ger" => "de",
        "ita" => "it",
        "por" => "pt",
        "nld" | "dut" => "nl",
        "jpn" => "ja",
        "zho" | "chi" => "zh",
        "kor" => "ko",
        "rus" => "ru",
        "ara" => "ar",
        "hin" => "hi",
        _ => "en",
    }
}

pub(super) fn compile(edit: &ValidatedEdit) -> Result<String> {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE fcpxml>\n<fcpxml version=\"1.10\">\n<resources>\n",
    );
    writeln!(
        out,
        "<format id=\"format0\" name=\"Skywrite\" frameDuration=\"{}\" width=\"{}\" height=\"{}\" colorSpace=\"1-1-1 (Rec. 709)\"/>",
        edit.fps.fcpxml(1),
        edit.width,
        edit.height
    )?;
    let mut assets = HashMap::<PathBuf, String>::new();
    let mut ordered = Vec::new();
    for (source, info, still) in edit
        .video_clips()
        .map(|c| (&c.source, &c.media, c.kind == MediaKind::Image))
        .chain(edit.audio_clips().map(|c| (&c.source, &c.media, false)))
    {
        if !assets.contains_key(source) {
            let id = format!("asset{}", assets.len());
            assets.insert(source.clone(), id.clone());
            ordered.push((id, source.clone(), info.clone(), still));
        }
    }
    for (index, (id, source, info, still)) in ordered.iter().enumerate() {
        if let Some(video) = info.video() {
            write!(
                out,
                "<format id=\"sourceFormat{index}\" width=\"{}\" height=\"{}\"",
                video.width.unwrap_or(edit.width),
                video.height.unwrap_or(edit.height)
            )?;
            if !still {
                let native = video
                    .avg_frame_rate
                    .as_deref()
                    .and_then(parse_fps)
                    .unwrap_or(edit.fps);
                write!(out, " frameDuration=\"{}\"", native.fcpxml(1))?;
            }
            writeln!(out, "/>")?;
        }
        let url = url::Url::from_file_path(source)
            .map_err(|_| anyhow::anyhow!("cannot make file URL for {}", source.display()))?;
        write!(
            out,
            "<asset id=\"{id}\" name=\"{}\" start=\"{}\" duration=\"{}\"",
            file_name(source, "Media"),
            seconds(info.start_time),
            if *still {
                "0s".into()
            } else {
                seconds(info.duration)
            }
        )?;
        if info.has_video() {
            write!(out, " hasVideo=\"1\" format=\"sourceFormat{index}\"")?;
        }
        if let Some(audio) = info.audio() {
            write!(
                out,
                " hasAudio=\"1\" audioSources=\"1\" audioChannels=\"{}\" audioRate=\"{}\"",
                audio.channels.unwrap_or(2),
                audio.sample_rate.unwrap_or(48000)
            )?;
        }
        writeln!(
            out,
            "><media-rep kind=\"original-media\" src=\"{}\"/></asset>",
            xml(url.as_str())
        )?;
    }
    writeln!(
        out,
        "</resources>\n<library><event name=\"Skywrite\"><project name=\"{}\"><sequence format=\"format0\" duration=\"{}\" tcStart=\"0s\" tcFormat=\"NDF\" audioLayout=\"stereo\" audioRate=\"48k\"><spine>",
        xml(&edit.name),
        edit.fps.fcpxml(edit.duration_frames)
    )?;
    // A full-length gap anchors every track as a connected storyline, so no
    // track's placement depends on another track's retiming.
    writeln!(
        out,
        "<gap name=\"Skywrite timeline\" offset=\"0s\" start=\"0s\" duration=\"{}\">",
        edit.fps.fcpxml(edit.duration_frames)
    )?;
    // Native FCPXML captions, connected above every video track, one lane per
    // caption track. Burn mode is a render setting and is not represented.
    let caption_base = edit.video_tracks().len() + 1;
    let mut style = 0usize;
    for (index, track) in edit.captions.iter().enumerate() {
        let role = format!("SRT?captionFormat=SRT.{}", two_letter(&track.language));
        for cue in &track.cues {
            style += 1;
            writeln!(
                out,
                "<caption lane=\"{}\" offset=\"{}\" name=\"{}\" start=\"0s\" duration=\"{}\" role=\"{}\"><text placement=\"bottom\"><text-style ref=\"cts{style}\">{}</text-style></text><text-style-def id=\"cts{style}\"><text-style font=\"Helvetica\" fontSize=\"13\" fontFace=\"Regular\" fontColor=\"1 1 1 1\" backgroundColor=\"0 0 0 1\"/></text-style-def></caption>",
                caption_base + index,
                seconds(cue.start),
                xml(&cue.text.chars().take(40).collect::<String>()),
                seconds(cue.end - cue.start),
                xml(&role),
                xml(&cue.text)
            )?;
        }
    }
    for (t, track) in edit.video_tracks().iter().enumerate() {
        if track.clips.is_empty() {
            continue;
        }
        writeln!(
            out,
            "<spine lane=\"{}\" offset=\"0s\" name=\"{}\">",
            t + 1,
            xml(&track.name)
        )?;
        let mut cursor = 0;
        for clip in &track.clips {
            gap(&mut out, edit, cursor, clip.start_frames)?;
            cursor = clip.end_frames();
            let duration = edit.fps.seconds(clip.duration_frames);
            let start = clip.media.start_time + clip.source_in;
            let local_start = if clip.speed == 1.0 && clip.kind == MediaKind::Video {
                start
            } else {
                0.0
            };
            let tag = if clip.kind == MediaKind::Image {
                "video"
            } else {
                "asset-clip"
            };
            write!(
                out,
                "<{tag} ref=\"{}\" name=\"{}\" offset=\"{}\" start=\"{}\" duration=\"{}\"",
                assets[&clip.source],
                file_name(&clip.source, "Clip"),
                edit.fps.fcpxml(clip.start_frames),
                seconds(local_start),
                edit.fps.fcpxml(clip.duration_frames)
            )?;
            if tag == "asset-clip" && !clip.has_audible_sound() {
                write!(out, " srcEnable=\"video\"")?;
            } else if let (true, Some(role)) = (tag == "asset-clip", track.role) {
                write!(out, " audioRole=\"{}\"", role.as_str())?;
            }
            writeln!(out, ">")?;
            if clip.speed != 1.0 {
                time_map(&mut out, start, duration, clip.speed)?;
            }
            blend(&mut out, clip, local_start, duration)?;
            if clip.has_audible_sound() {
                volume(&mut out, &clip.audio, local_start)?;
            }
            writeln!(out, "</{tag}>")?;
        }
        writeln!(out, "</spine>")?;
    }
    for (t, track) in edit.audio_tracks().iter().enumerate() {
        if track.clips.is_empty() {
            continue;
        }
        writeln!(
            out,
            "<spine lane=\"-{}\" offset=\"0s\" name=\"{}\">",
            t + 1,
            xml(&track.name)
        )?;
        let mut cursor = 0;
        for clip in &track.clips {
            gap(&mut out, edit, cursor, clip.start_frames)?;
            cursor = clip.end_frames();
            let start = clip.media.start_time + clip.source_in;
            let local_start = if clip.speed == 1.0 { start } else { 0.0 };
            writeln!(
                out,
                "<asset-clip ref=\"{}\" name=\"{}\" offset=\"{}\" start=\"{}\" duration=\"{}\" audioRole=\"{}\" srcEnable=\"audio\"{}>",
                assets[&clip.source],
                file_name(&clip.source, "Audio"),
                edit.fps.fcpxml(clip.start_frames),
                seconds(local_start),
                edit.fps.fcpxml(clip.duration_frames),
                // FCP's default audio role is dialogue; a declared role wins.
                track.role.map_or("dialogue", |r| r.as_str()),
                if clip.mix.audible() {
                    ""
                } else {
                    " enabled=\"0\""
                }
            )?;
            if clip.speed != 1.0 {
                time_map(
                    &mut out,
                    start,
                    edit.fps.seconds(clip.duration_frames),
                    clip.speed,
                )?;
            }
            volume(&mut out, &clip.mix, local_start)?;
            writeln!(out, "</asset-clip>")?;
        }
        writeln!(out, "</spine>")?;
    }
    for chapter in &edit.chapters {
        writeln!(
            out,
            "<chapter-marker start=\"{}\" duration=\"{}\" value=\"{}\" posterOffset=\"0s\"/>",
            seconds(chapter.start),
            edit.fps.fcpxml(1),
            xml(&chapter.title)
        )?;
    }
    writeln!(
        out,
        "</gap></spine></sequence></project></event></library></fcpxml>"
    )?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rational_source_time() {
        assert_eq!(seconds(-1.25), "-5/4s");
        assert_eq!(seconds(0.5), "1/2s");
    }
}
