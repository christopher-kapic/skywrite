//! FCPXML 1.10, following Apple's FCPXML reference and timeMap specification.
use super::xml;
use crate::timeline::{self, MediaKind, Rational, ValidatedEdit};
use anyhow::{Context, Result};
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
    for clip in &edit.clips {
        if let (Some(source), Some(info)) = (&clip.source, &clip.media)
            && !assets.contains_key(source)
        {
            let id = format!("asset{}", assets.len());
            assets.insert(source.clone(), id.clone());
            ordered.push((
                id,
                source.clone(),
                info.clone(),
                clip.kind == MediaKind::Image,
            ));
        }
    }
    for clip in &edit.audio {
        if !assets.contains_key(&clip.source) {
            let id = format!("asset{}", assets.len());
            assets.insert(clip.source.clone(), id.clone());
            ordered.push((id, clip.source.clone(), clip.media.clone(), false));
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
        let name = xml(source
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("Media"));
        write!(
            out,
            "<asset id=\"{id}\" name=\"{name}\" start=\"{}\" duration=\"{}\"",
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
    // A full-length gap anchors the video storyline and independent audio lanes.
    // External audio placement remains independent of a retimed first shot.
    writeln!(
        out,
        "<gap name=\"Skywrite assembly\" offset=\"0s\" start=\"0s\" duration=\"{}\"><spine lane=\"1\" offset=\"0s\">",
        edit.fps.fcpxml(edit.duration_frames)
    )?;
    for clip in &edit.clips {
        let duration = edit.fps.fcpxml(clip.duration_frames);
        let offset = edit.fps.fcpxml(clip.offset_frames);
        if clip.kind == MediaKind::Gap {
            writeln!(
                out,
                "<gap name=\"Gap\" offset=\"{offset}\" start=\"0s\" duration=\"{duration}\"/>"
            )?;
            continue;
        }
        let source = clip.source.as_ref().context("validated source missing")?;
        let info = clip.media.as_ref().context("validated media missing")?;
        let start = info.start_time + clip.source_in;
        let local_start = if clip.speed == 1.0 { start } else { 0.0 };
        let tag = if clip.kind == MediaKind::Image {
            "video"
        } else {
            "asset-clip"
        };
        write!(
            out,
            "<{tag} ref=\"{}\" name=\"{}\" offset=\"{offset}\" start=\"{}\" duration=\"{duration}\"",
            assets[source],
            xml(source
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("Clip")),
            seconds(local_start)
        )?;
        if tag == "asset-clip" && clip.volume == 0.0 {
            write!(out, " srcEnable=\"video\"")?;
        }
        writeln!(out, ">")?;
        if clip.speed != 1.0 {
            time_map(
                &mut out,
                start,
                edit.fps.seconds(clip.duration_frames),
                clip.speed,
            )?;
        }
        if clip.volume > 0.0 && clip.volume != 1.0 && info.has_audio() {
            writeln!(
                out,
                "<adjust-volume amount=\"{:.6}dB\"/>",
                20.0 * clip.volume.log10()
            )?;
        }
        writeln!(out, "</{tag}>")?;
    }
    writeln!(out, "</spine>")?;
    for (index, clip) in edit.audio.iter().enumerate() {
        let start = clip.media.start_time + clip.source_in;
        let local_start = if clip.speed == 1.0 { start } else { 0.0 };
        writeln!(
            out,
            "<asset-clip ref=\"{}\" lane=\"-{}\" name=\"{}\" offset=\"{}\" start=\"{}\" duration=\"{}\" audioRole=\"dialogue\" srcEnable=\"audio\"{}>",
            assets[&clip.source],
            index + 1,
            xml(clip
                .source
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("Audio")),
            edit.fps.fcpxml(clip.offset_frames),
            seconds(local_start),
            edit.fps.fcpxml(clip.duration_frames),
            if clip.volume == 0.0 {
                " enabled=\"0\""
            } else {
                ""
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
        if clip.volume > 0.0 && clip.volume != 1.0 {
            writeln!(
                out,
                "<adjust-volume amount=\"{:.6}dB\"/>",
                20.0 * clip.volume.log10()
            )?;
        }
        writeln!(out, "</asset-clip>")?;
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
