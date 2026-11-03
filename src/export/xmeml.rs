//! Final Cut Pro 7 XML ("xmeml" version 5), the interchange Premiere Pro imports.
//!
//! Every video track and its linked sound, plus every audio track, keeps its
//! own layer. Speed, opacity, fades and gain envelopes are written as the
//! standard Time Remap, Opacity and Audio Levels effects. Timing uses whole
//! frames at the sequence rate, so subframe source in points are rounded.
use super::xml;
use crate::timeline::{
    Keyframe, MediaKind, Rational, ValidatedEdit, ValidatedMix, ValidatedVideoClip,
};
use anyhow::{Result, bail};
use std::{collections::HashMap, fmt::Write as _, path::Path};

struct Rate {
    timebase: u32,
    ntsc: bool,
}

fn rate(fps: Rational) -> Result<Rate> {
    if fps.num.is_multiple_of(fps.den) {
        return Ok(Rate {
            timebase: fps.num / fps.den,
            ntsc: false,
        });
    }
    // NTSC rates are N*1000/1001, possibly written unreduced.
    let (num, den) = (fps.num as u64, fps.den as u64);
    if (num * 1001).is_multiple_of(den * 1000) {
        let timebase = num * 1001 / (den * 1000);
        if timebase > 0 {
            return Ok(Rate {
                timebase: timebase as u32,
                ntsc: true,
            });
        }
    }
    bail!(
        "Premiere XML supports integer or NTSC (N*1000/1001) frame rates, not {}/{}",
        fps.num,
        fps.den
    )
}

fn rate_xml(rate: &Rate) -> String {
    format!(
        "<rate><timebase>{}</timebase><ntsc>{}</ntsc></rate>",
        rate.timebase,
        if rate.ntsc { "TRUE" } else { "FALSE" }
    )
}

fn path_url(path: &Path) -> Result<String> {
    let url = url::Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("cannot make file URL for {}", path.display()))?;
    // Premiere writes and expects the explicit localhost authority.
    Ok(url.as_str().replacen("file://", "file://localhost", 1))
}

fn name(path: &Path) -> String {
    xml(path.file_name().and_then(|v| v.to_str()).unwrap_or("Media"))
}

struct Files<'a> {
    ids: HashMap<&'a Path, usize>,
}
impl<'a> Files<'a> {
    /// First use carries the full definition; later uses reference its id.
    fn element(
        &mut self,
        path: &'a Path,
        info: &crate::media::MediaInfo,
        still_frames: Option<u64>,
        fps: Rational,
        rate: &Rate,
    ) -> Result<String> {
        if let Some(id) = self.ids.get(path) {
            return Ok(format!("<file id=\"file-{id}\"/>"));
        }
        let id = self.ids.len() + 1;
        self.ids.insert(path, id);
        let frames = still_frames.unwrap_or_else(|| fps.frames(info.duration));
        let mut media = String::new();
        if let Some(video) = info.video() {
            write!(
                media,
                "<video><samplecharacteristics>{}<width>{}</width><height>{}</height></samplecharacteristics></video>",
                rate_xml(rate),
                video.width.unwrap_or(0),
                video.height.unwrap_or(0)
            )?;
        }
        if let Some(audio) = info.audio() {
            write!(
                media,
                "<audio><samplecharacteristics><depth>16</depth><samplerate>{}</samplerate></samplecharacteristics><channelcount>{}</channelcount></audio>",
                audio.sample_rate.unwrap_or(48000),
                audio.channels.unwrap_or(2)
            )?;
        }
        Ok(format!(
            "<file id=\"file-{id}\"><name>{}</name><pathurl>{}</pathurl>{}<duration>{frames}</duration><timecode>{}<string>00:00:00:00</string><frame>0</frame><displayformat>NDF</displayformat></timecode><media>{media}</media></file>",
            name(path),
            xml(&path_url(path)?),
            rate_xml(rate),
            rate_xml(rate)
        ))
    }
}

fn time_remap(speed: f64, media: &str) -> String {
    format!(
        "<filter><effect><name>Time Remap</name><effectid>timeremap</effectid><effectcategory>motion</effectcategory><effecttype>motion</effecttype><mediatype>{media}</mediatype><parameter authoringApp=\"PremierePro\"><parameterid>variablespeed</parameterid><name>variablespeed</name><valuemin>0</valuemin><valuemax>1</valuemax><value>0</value></parameter><parameter authoringApp=\"PremierePro\"><parameterid>speed</parameterid><name>speed</name><valuemin>-100000</valuemin><valuemax>100000</valuemax><value>{:.6}</value></parameter><parameter authoringApp=\"PremierePro\"><parameterid>reverse</parameterid><name>reverse</name><value>FALSE</value></parameter><parameter authoringApp=\"PremierePro\"><parameterid>frameblending</parameterid><name>frameblending</name><value>FALSE</value></parameter></effect></filter>",
        speed * 100.0
    )
}

fn keyframes(points: &[(u64, f64)]) -> String {
    points
        .iter()
        .map(|(when, value)| {
            format!("<keyframe><when>{when}</when><value>{value:.6}</value></keyframe>")
        })
        .collect()
}

/// Keyframe `when` values are clip media frames, offset by the in point.
fn opacity(clip: &ValidatedVideoClip, fps: Rational, in_frame: u64) -> String {
    if clip.opacity == 1.0 && clip.fade_in == 0.0 && clip.fade_out == 0.0 {
        return String::new();
    }
    let duration = fps.seconds(clip.duration_frames);
    let mut points = Vec::new();
    if clip.fade_in > 0.0 || clip.fade_out > 0.0 {
        let mut times = vec![(
            0.0,
            if clip.fade_in > 0.0 {
                0.0
            } else {
                clip.opacity
            },
        )];
        if clip.fade_in > 0.0 {
            times.push((clip.fade_in, clip.opacity));
        }
        if clip.fade_out > 0.0 {
            times.push((duration - clip.fade_out, clip.opacity));
        }
        times.push((
            duration,
            if clip.fade_out > 0.0 {
                0.0
            } else {
                clip.opacity
            },
        ));
        let mut last = None;
        for (time, value) in times {
            let when = in_frame + fps.frames(time);
            if last != Some(when) {
                points.push((when, value * 100.0));
                last = Some(when);
            }
        }
    }
    format!(
        "<filter><effect><name>Opacity</name><effectid>opacity</effectid><effectcategory>motion</effectcategory><effecttype>motion</effecttype><mediatype>video</mediatype><parameter authoringApp=\"PremierePro\"><parameterid>opacity</parameterid><name>opacity</name><valuemin>0</valuemin><valuemax>100</valuemax><value>{:.6}</value>{}</parameter></effect></filter>",
        clip.opacity * 100.0,
        keyframes(&points)
    )
}

fn levels(mix: &ValidatedMix, fps: Rational, in_frame: u64, duration: f64) -> String {
    if mix.is_unity() {
        return String::new();
    }
    let mut points: Vec<(u64, f64)> = Vec::new();
    if !(mix.keyframes.is_empty() && mix.fade_in == 0.0 && mix.fade_out == 0.0) {
        for Keyframe { time, volume } in mix.breakpoints(duration) {
            let when = in_frame + fps.frames(time);
            match points.last_mut() {
                Some(last) if last.0 == when => last.1 = volume,
                _ => points.push((when, volume)),
            }
        }
    }
    format!(
        "<filter><effect><name>Audio Levels</name><effectid>audiolevels</effectid><effectcategory>audiolevels</effectcategory><effecttype>audiolevels</effecttype><mediatype>audio</mediatype><parameter><parameterid>level</parameterid><name>Level</name><valuemin>0</valuemin><valuemax>3.98109</valuemax><value>{:.6}</value>{}</parameter></effect></filter>",
        mix.volume.min(3.98109),
        keyframes(&points)
    )
}

struct Item {
    id: String,
    track: usize,
    clip_index: usize,
    video: bool,
}

fn link(item: &Item) -> String {
    format!(
        "<link><linkclipref>{}</linkclipref><mediatype>{}</mediatype><trackindex>{}</trackindex><clipindex>{}</clipindex>{}</link>",
        item.id,
        if item.video { "video" } else { "audio" },
        item.track,
        item.clip_index,
        if item.video {
            ""
        } else {
            "<groupindex>1</groupindex>"
        }
    )
}

/// Returns XML plus notes about rounding and unverified mappings.
pub(super) fn compile(edit: &ValidatedEdit) -> Result<(String, Vec<String>)> {
    let fps = edit.fps;
    let rate = rate(fps)?;
    let mut notes = Vec::new();
    let mut files = Files {
        ids: HashMap::new(),
    };
    let mut next_id = 0usize;
    let mut new_id = || {
        next_id += 1;
        format!("clipitem-{next_id}")
    };
    let mut subframe = false;
    let mut retimed = false;
    let mut video_tracks = Vec::new();
    let mut linked_tracks = Vec::new();
    for (t, track) in edit.video_tracks().iter().enumerate() {
        let mut video_xml = String::new();
        let mut audio_xml = String::new();
        let audio_track_number = linked_tracks.len() + 1;
        let mut audio_index = 0usize;
        for (k, clip) in track.clips.iter().enumerate() {
            let duration = fps.seconds(clip.duration_frames);
            let (in_frame, item_duration) = if clip.kind == MediaKind::Image {
                (0, clip.duration_frames)
            } else {
                let exact = clip.source_in * fps.value() / clip.speed;
                subframe |= (exact - exact.round()).abs() > 1e-6;
                retimed |= clip.speed != 1.0;
                (
                    exact.round() as u64,
                    (fps.frames(clip.media.duration) as f64 / clip.speed).round() as u64,
                )
            };
            let out_frame = in_frame + clip.duration_frames;
            let video_item = Item {
                id: new_id(),
                track: t + 1,
                clip_index: k + 1,
                video: true,
            };
            let audio_item = clip.has_audible_sound().then(|| {
                audio_index += 1;
                Item {
                    id: new_id(),
                    track: audio_track_number,
                    clip_index: audio_index,
                    video: false,
                }
            });
            // A still's file is as long as the whole sequence, so every use of
            // it (including a later, longer one) fits inside the file.
            let still = (clip.kind == MediaKind::Image).then_some(edit.duration_frames);
            let file = files.element(&clip.source, &clip.media, still, fps, &rate)?;
            let timing = format!(
                "<duration>{item_duration}</duration>{}<start>{}</start><end>{}</end><in>{in_frame}</in><out>{out_frame}</out>",
                rate_xml(&rate),
                clip.start_frames,
                clip.end_frames()
            );
            let mut links = link(&video_item);
            if let Some(audio) = &audio_item {
                links.push_str(&link(audio));
            }
            write!(
                video_xml,
                "<clipitem id=\"{}\"><name>{}</name><enabled>TRUE</enabled>{timing}{}{file}{}{}{links}</clipitem>",
                video_item.id,
                name(&clip.source),
                if clip.kind == MediaKind::Image {
                    "<stillframe>TRUE</stillframe>"
                } else {
                    ""
                },
                if clip.speed != 1.0 {
                    time_remap(clip.speed, "video")
                } else {
                    String::new()
                },
                opacity(clip, fps, in_frame),
            )?;
            if let Some(audio) = &audio_item {
                write!(
                    audio_xml,
                    "<clipitem id=\"{}\" premiereChannelType=\"{}\"><name>{}</name><enabled>TRUE</enabled>{timing}<file id=\"file-{}\"/><sourcetrack><mediatype>audio</mediatype><trackindex>1</trackindex></sourcetrack>{}{}{links}</clipitem>",
                    audio.id,
                    channel_type(&clip.media),
                    name(&clip.source),
                    files.ids[clip.source.as_path()],
                    if clip.speed != 1.0 {
                        time_remap(clip.speed, "audio")
                    } else {
                        String::new()
                    },
                    levels(&clip.audio, fps, in_frame, duration),
                )?;
            }
        }
        video_tracks.push(format!(
            "<track>{video_xml}<enabled>TRUE</enabled><locked>FALSE</locked></track>"
        ));
        if audio_index > 0 {
            linked_tracks.push(format!(
                "<track premiereTrackType=\"Stereo\">{audio_xml}<enabled>TRUE</enabled><locked>FALSE</locked></track>"
            ));
        }
    }
    let mut audio_tracks = linked_tracks;
    for track in edit.audio_tracks() {
        let mut audio_xml = String::new();
        let track_number = audio_tracks.len() + 1;
        for (position, clip) in track.clips.iter().filter(|c| c.mix.audible()).enumerate() {
            let duration = fps.seconds(clip.duration_frames);
            let exact = clip.source_in * fps.value() / clip.speed;
            subframe |= (exact - exact.round()).abs() > 1e-6;
            retimed |= clip.speed != 1.0;
            let in_frame = exact.round() as u64;
            let item = Item {
                id: new_id(),
                track: track_number,
                clip_index: position + 1,
                video: false,
            };
            let file = files.element(&clip.source, &clip.media, None, fps, &rate)?;
            write!(
                audio_xml,
                "<clipitem id=\"{}\" premiereChannelType=\"{}\"><name>{}</name><enabled>TRUE</enabled><duration>{}</duration>{}<start>{}</start><end>{}</end><in>{in_frame}</in><out>{}</out>{file}<sourcetrack><mediatype>audio</mediatype><trackindex>1</trackindex></sourcetrack>{}{}{}</clipitem>",
                item.id,
                channel_type(&clip.media),
                name(&clip.source),
                (fps.frames(clip.media.duration) as f64 / clip.speed).round() as u64,
                rate_xml(&rate),
                clip.start_frames,
                clip.end_frames(),
                in_frame + clip.duration_frames,
                if clip.speed != 1.0 {
                    time_remap(clip.speed, "audio")
                } else {
                    String::new()
                },
                levels(&clip.mix, fps, in_frame, duration),
                link(&item),
            )?;
        }
        audio_tracks.push(format!(
            "<track premiereTrackType=\"Stereo\">{audio_xml}<enabled>TRUE</enabled><locked>FALSE</locked></track>"
        ));
    }
    if subframe {
        notes.push("Premiere XML uses whole frames: subframe source in points (word-precise cuts) were rounded to the nearest frame. Listen to dialogue cuts after import.".into());
    }
    if retimed {
        notes.push("Retimed clips use the Time Remap speed effect with in/out points in retimed frames; check speed and sync after Premiere import.".into());
    }
    notes.push("Premiere import of this XML has not been verified by Skywrite. Please record the result with skywrite nit, including any import errors or timing differences.".into());
    let markers: String = edit
        .chapters
        .iter()
        .map(|c| {
            format!(
                "<marker><name>{}</name><comment></comment><in>{}</in><out>-1</out></marker>",
                xml(&c.title),
                fps.frames(c.start)
            )
        })
        .collect();
    let out = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE xmeml>\n<xmeml version=\"5\"><sequence id=\"sequence-1\"><name>{name}</name><duration>{duration}</duration>{rate}<timecode>{rate}<string>00:00:00:00</string><frame>0</frame><displayformat>NDF</displayformat></timecode><media><video><format><samplecharacteristics>{rate}<width>{width}</width><height>{height}</height><anamorphic>FALSE</anamorphic><pixelaspectratio>square</pixelaspectratio><fielddominance>none</fielddominance></samplecharacteristics></format>{video}</video><audio><numOutputChannels>2</numOutputChannels><format><samplecharacteristics><depth>16</depth><samplerate>48000</samplerate></samplecharacteristics></format>{audio}</audio></media>{markers}</sequence></xmeml>\n",
        name = xml(&edit.name),
        duration = edit.duration_frames,
        rate = rate_xml(&rate),
        width = edit.width,
        height = edit.height,
        video = video_tracks.join(""),
        audio = audio_tracks.join(""),
    );
    Ok((out, notes))
}

fn channel_type(info: &crate::media::MediaInfo) -> &'static str {
    match info.audio().and_then(|a| a.channels) {
        Some(1) => "mono",
        _ => "stereo",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rates() {
        let r = rate(Rational {
            num: 30000,
            den: 1001,
        })
        .unwrap();
        assert_eq!((r.timebase, r.ntsc), (30, true));
        let r = rate(Rational { num: 24, den: 1 }).unwrap();
        assert_eq!((r.timebase, r.ntsc), (24, false));
        let r = rate(Rational {
            num: 60000,
            den: 1001,
        })
        .unwrap();
        assert_eq!((r.timebase, r.ntsc), (60, true));
        assert!(rate(Rational { num: 25, den: 2 }).is_err());
    }
    #[test]
    fn localhost_urls() {
        assert_eq!(
            path_url(Path::new("/tmp/a b.mp4")).unwrap(),
            "file://localhost/tmp/a%20b.mp4"
        );
    }
}
