//! The validated, layered edit format shared by every output backend.
//!
//! Edit version 2 places every clip explicitly on a track. Times are seconds:
//! `start` is a position on the output timeline, `source_in` is measured from
//! the source's first presented sample, `duration` is output duration, and
//! `speed` is source seconds per output second. Video track 0 is the bottom
//! layer; later tracks composite above it. Empty timeline regions are black and
//! silent. Version 1 edits are rejected with a pointer to `edit migrate`.
use crate::{
    captions::{self, CaptionTrack, Chapter, ValidatedTrack},
    io, media,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub const EDIT_VERSION: u32 = 2;
pub const MAX_VIDEO_TRACKS: usize = 16;
pub const MAX_AUDIO_TRACKS: usize = 32;
pub const MAX_VIDEO_CLIPS: usize = 1024;
pub const MAX_AUDIO_CLIPS: usize = 1024;
pub const MAX_KEYFRAMES: usize = 256;
pub const MAX_LUT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TIMELINE_SECONDS: f64 = 86_400.0;

fn one() -> f64 {
    1.0
}
fn default_name() -> String {
    "Skywrite edit".into()
}
fn default_width() -> u32 {
    1920
}
fn default_height() -> u32 {
    1080
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rational {
    pub num: u32,
    pub den: u32,
}
impl Default for Rational {
    fn default() -> Self {
        Self { num: 30, den: 1 }
    }
}
impl Rational {
    pub fn value(self) -> f64 {
        self.num as f64 / self.den as f64
    }
    pub fn frames(self, seconds: f64) -> u64 {
        (seconds * self.value()).round() as u64
    }
    pub fn seconds(self, frames: u64) -> f64 {
        frames as f64 / self.value()
    }
    pub fn ffmpeg(self) -> String {
        format!("{}/{}", self.num, self.den)
    }
    pub fn fcpxml(self, frames: u64) -> String {
        rational_time(frames * self.den as u64, self.num as u64)
    }
    /// Sample index at 48 kHz for a frame boundary; shared by every audio path
    /// so adjacent segments meet exactly without cumulative rounding drift.
    pub fn samples_48k(self, frames: u64) -> u64 {
        ((frames as u128 * 48_000 * self.den as u128 + self.num as u128 / 2) / self.num as u128)
            as u64
    }
}

/// Rational seconds for interchange, reduced to avoid unnecessarily large values.
pub fn rational_time(num: u64, den: u64) -> String {
    fn gcd(mut a: u64, mut b: u64) -> u64 {
        while b != 0 {
            let r = a % b;
            a = b;
            b = r;
        }
        a
    }
    let divisor = gcd(num, den).max(1);
    if den / divisor == 1 {
        format!("{}s", num / divisor)
    } else {
        format!("{}/{}s", num / divisor, den / divisor)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub version: u32,
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default)]
    pub fps: Rational,
    /// Explicit timeline length; defaults to the latest clip end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Per-source settings such as a declared color encoding and a LUT.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceSettings>,
    /// Video tracks, bottom layer first.
    #[serde(default)]
    pub video: Vec<VideoTrack>,
    /// Audio-only tracks, mixed with linked audio from video clips.
    #[serde(default)]
    pub audio: Vec<AudioTrack>,
    #[serde(default, skip_serializing_if = "Master::is_default")]
    pub master: Master,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captions: Vec<CaptionTrack>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chapters: Vec<Chapter>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Master {
    /// Optional output limiter ceiling in dBFS (-24 through 0). Without it the
    /// mix is summed at unity and can clip; no hidden limiter is applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limiter_db: Option<f64>,
}
impl Master {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSettings {
    pub path: PathBuf,
    /// A `.cube` LUT applied to this source's pictures in rendered output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lut: Option<PathBuf>,
    /// The true capture encoding, as confirmed by the operator. Overrides
    /// container metadata in reports; it does not itself transform pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<ColorDeclaration>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ColorDeclaration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primaries: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoTrack {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Role of this track's linked sound (dialogue, music or effects).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(default)]
    pub clips: Vec<VideoClip>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioTrack {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(default)]
    pub clips: Vec<AudioClip>,
}

/// What a track's sound is, for stems, NLE audio roles and review checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Dialogue,
    Music,
    Effects,
}
impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Dialogue => "dialogue",
            Role::Music => "music",
            Role::Effects => "effects",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoClip {
    pub start: f64,
    pub source: PathBuf,
    #[serde(default)]
    pub source_in: f64,
    pub duration: f64,
    #[serde(default = "one")]
    pub speed: f64,
    /// Picture opacity, 0 through 1, over lower tracks (black at the bottom).
    #[serde(default = "one")]
    pub opacity: f64,
    /// Picture fade from transparent, in output seconds.
    #[serde(default)]
    pub fade_in: f64,
    /// Picture fade to transparent, in output seconds.
    #[serde(default)]
    pub fade_out: f64,
    /// The clip's own (linked) sound.
    #[serde(default)]
    pub audio: Mix,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioClip {
    pub start: f64,
    pub source: PathBuf,
    #[serde(default)]
    pub source_in: f64,
    pub duration: f64,
    #[serde(default = "one")]
    pub speed: f64,
    #[serde(default = "one")]
    pub volume: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_keyframes: Vec<Keyframe>,
    #[serde(default)]
    pub fade_in: f64,
    #[serde(default)]
    pub fade_out: f64,
}

/// Gain for a clip's sound. Effective gain is `volume`, times the keyframe
/// envelope when present, times fades.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Mix {
    #[serde(default = "one")]
    pub volume: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_keyframes: Vec<Keyframe>,
    #[serde(default)]
    pub fade_in: f64,
    #[serde(default)]
    pub fade_out: f64,
}
impl Default for Mix {
    fn default() -> Self {
        Self {
            volume: 1.0,
            volume_keyframes: Vec::new(),
            fade_in: 0.0,
            fade_out: 0.0,
        }
    }
}

/// A linear gain point, `time` in output seconds from the clip's start.
/// Gain is linearly interpolated between points and held outside them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Keyframe {
    pub time: f64,
    pub volume: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Video,
    Image,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidatedMix {
    pub volume: f64,
    pub keyframes: Vec<Keyframe>,
    pub fade_in: f64,
    pub fade_out: f64,
}
impl ValidatedMix {
    /// Fully muted sound.
    pub fn silent() -> Self {
        Self {
            volume: 0.0,
            keyframes: Vec::new(),
            fade_in: 0.0,
            fade_out: 0.0,
        }
    }
    /// True when any part of the clip can be heard.
    pub fn audible(&self) -> bool {
        self.volume > 0.0
            && (self.keyframes.is_empty() || self.keyframes.iter().any(|k| k.volume > 0.0))
    }
    pub fn is_unity(&self) -> bool {
        self.volume == 1.0
            && self.keyframes.is_empty()
            && self.fade_in == 0.0
            && self.fade_out == 0.0
    }
    /// Envelope gain at clip-relative output seconds, excluding fades.
    pub fn envelope(&self, time: f64) -> f64 {
        let Some(first) = self.keyframes.first() else {
            return self.volume;
        };
        let last = self.keyframes.last().expect("non-empty");
        let shape = if time <= first.time {
            first.volume
        } else if time >= last.time {
            last.volume
        } else {
            let index = self.keyframes.partition_point(|k| k.time <= time);
            let (a, b) = (self.keyframes[index - 1], self.keyframes[index]);
            a.volume + (b.volume - a.volume) * (time - a.time) / (b.time - a.time)
        };
        self.volume * shape
    }
    /// Full gain including linear fades.
    pub fn gain(&self, time: f64, duration: f64) -> f64 {
        let mut gain = self.envelope(time);
        if self.fade_in > 0.0 && time < self.fade_in {
            gain *= (time / self.fade_in).clamp(0.0, 1.0);
        }
        if self.fade_out > 0.0 && time > duration - self.fade_out {
            gain *= ((duration - time) / self.fade_out).clamp(0.0, 1.0);
        }
        gain
    }
    /// Breakpoints of the full gain curve, for NLE keyframe export.
    pub fn breakpoints(&self, duration: f64) -> Vec<Keyframe> {
        let mut times = vec![0.0, duration];
        times.extend(self.keyframes.iter().map(|k| k.time));
        if self.fade_in > 0.0 {
            times.push(self.fade_in);
        }
        if self.fade_out > 0.0 {
            times.push(duration - self.fade_out);
        }
        // Where a fade overlaps a keyframe ramp the product is curved; sample
        // it so linear NLE interpolation stays within a small error.
        if !self.keyframes.is_empty() {
            for (from, to) in [(0.0, self.fade_in), (duration - self.fade_out, duration)] {
                let span = to - from;
                if span > 0.0 {
                    let steps = ((span / 0.1).ceil() as usize).clamp(1, 100);
                    times.extend((1..steps).map(|i| from + span * i as f64 / steps as f64));
                }
            }
        }
        times.retain(|t| (0.0..=duration).contains(t));
        times.sort_by(f64::total_cmp);
        times.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
        times
            .into_iter()
            .map(|time| Keyframe {
                time,
                volume: self.gain(time, duration),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidatedVideoClip {
    pub track: usize,
    pub index: usize,
    pub source: PathBuf,
    pub kind: MediaKind,
    pub source_in: f64,
    pub start_frames: u64,
    pub duration_frames: u64,
    pub speed: f64,
    pub opacity: f64,
    pub fade_in: f64,
    pub fade_out: f64,
    pub audio: ValidatedMix,
    pub media: media::MediaInfo,
    /// Index into `ValidatedEdit::luts`.
    pub lut: Option<usize>,
}
impl ValidatedVideoClip {
    pub fn has_audible_sound(&self) -> bool {
        self.kind == MediaKind::Video && self.media.has_audio() && self.audio.audible()
    }
    pub fn end_frames(&self) -> u64 {
        self.start_frames + self.duration_frames
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidatedAudioClip {
    pub track: usize,
    pub index: usize,
    pub source: PathBuf,
    pub source_in: f64,
    pub start_frames: u64,
    pub duration_frames: u64,
    pub speed: f64,
    pub mix: ValidatedMix,
    pub media: media::MediaInfo,
}
impl ValidatedAudioClip {
    pub fn end_frames(&self) -> u64 {
        self.start_frames + self.duration_frames
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidatedTrackInfo<T> {
    pub name: String,
    pub role: Option<Role>,
    pub clips: Vec<T>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidatedLut {
    pub path: PathBuf,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidatedSource {
    pub path: PathBuf,
    pub color: Option<ColorDeclaration>,
    pub lut: Option<PathBuf>,
}

/// A span of audible source sound on the output timeline.
#[derive(Debug, Clone)]
pub struct AudibleSpan<'a> {
    pub label: String,
    pub source: &'a Path,
    pub source_in: f64,
    pub start: f64,
    pub duration: f64,
    pub speed: f64,
    pub mix: &'a ValidatedMix,
    pub role: Option<Role>,
}

/// Constructed only by the validator; exporters cannot accidentally trust raw JSON.
#[derive(Debug, Clone, Serialize)]
pub struct ValidatedEdit {
    pub(crate) name: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: Rational,
    pub(crate) duration_frames: u64,
    pub(crate) video: Vec<ValidatedTrackInfo<ValidatedVideoClip>>,
    pub(crate) audio: Vec<ValidatedTrackInfo<ValidatedAudioClip>>,
    pub(crate) master: Master,
    pub(crate) luts: Vec<ValidatedLut>,
    pub(crate) sources: Vec<ValidatedSource>,
    pub(crate) warnings: Vec<String>,
    pub(crate) captions: Vec<ValidatedTrack>,
    pub(crate) chapters: Vec<Chapter>,
    pub(crate) inputs: Vec<PathBuf>,
}
impl ValidatedEdit {
    pub fn duration(&self) -> f64 {
        self.fps.seconds(self.duration_frames)
    }
    pub fn fps(&self) -> Rational {
        self.fps
    }
    pub fn duration_frames(&self) -> u64 {
        self.duration_frames
    }
    pub fn video_clips(&self) -> impl Iterator<Item = &ValidatedVideoClip> {
        self.video.iter().flat_map(|t| t.clips.iter())
    }
    pub fn audio_clips(&self) -> impl Iterator<Item = &ValidatedAudioClip> {
        self.audio.iter().flat_map(|t| t.clips.iter())
    }
    pub fn video_tracks(&self) -> &[ValidatedTrackInfo<ValidatedVideoClip>] {
        &self.video
    }
    pub fn audio_tracks(&self) -> &[ValidatedTrackInfo<ValidatedAudioClip>] {
        &self.audio
    }
    pub fn summary(&self) -> Value {
        json!({"version":EDIT_VERSION,"valid":true,"name":self.name,"width":self.width,"height":self.height,"fps":self.fps,
          "duration_seconds":self.duration(),"duration_frames":self.duration_frames,
          "video_tracks":self.video.iter().map(|t|json!({"name":t.name,"role":t.role,"clip_count":t.clips.len()})).collect::<Vec<_>>(),
          "audio_tracks":self.audio.iter().map(|t|json!({"name":t.name,"role":t.role,"clip_count":t.clips.len()})).collect::<Vec<_>>(),
          "video_clip_count":self.video_clips().count(),"audio_clip_count":self.audio_clips().count(),
          "luts":self.luts.iter().map(|l|&l.path).collect::<Vec<_>>(),"master":self.master,
          "caption_track_count":self.captions.len(),"chapter_count":self.chapters.len(),"warnings":self.warnings})
    }
    pub fn protected_paths(&self) -> impl Iterator<Item = &Path> {
        self.source_paths()
            .chain(self.inputs.iter().map(PathBuf::as_path))
            .chain(self.luts.iter().map(|l| l.path.as_path()))
            .chain(self.captions.iter().map(|c| c.source.as_path()))
            .chain(
                self.captions
                    .iter()
                    .flat_map(|c| c.inputs.iter().map(PathBuf::as_path)),
            )
    }
    pub fn source_paths(&self) -> impl Iterator<Item = &Path> {
        self.video_clips()
            .map(|c| c.source.as_path())
            .chain(self.audio_clips().map(|a| a.source.as_path()))
    }
    /// A copy that keeps only the sound of tracks with one of `roles`; pictures
    /// are unchanged. Every audible track must declare a role, so nothing is
    /// silently dropped or kept by guesswork.
    pub fn with_roles(&self, roles: &[Role]) -> Result<ValidatedEdit> {
        let unroled: Vec<&str> = self
            .video
            .iter()
            .filter(|t| t.role.is_none() && t.clips.iter().any(|c| c.has_audible_sound()))
            .map(|t| t.name.as_str())
            .chain(
                self.audio
                    .iter()
                    .filter(|t| t.role.is_none() && t.clips.iter().any(|c| c.mix.audible()))
                    .map(|t| t.name.as_str()),
            )
            .collect();
        ensure!(
            unroled.is_empty(),
            "--roles needs a role on every audible track; add \"role\": \"dialogue\" | \"music\" | \"effects\" to {}",
            unroled.join(", ")
        );
        let mut edit = self.clone();
        for track in &mut edit.video {
            if !track.role.is_some_and(|r| roles.contains(&r)) {
                for clip in &mut track.clips {
                    clip.audio = ValidatedMix::silent();
                }
            }
        }
        for track in &mut edit.audio {
            if !track.role.is_some_and(|r| roles.contains(&r)) {
                track.clips.clear();
            }
        }
        Ok(edit)
    }
    /// Every clip whose sound can be heard, in track order.
    pub fn audible_spans(&self) -> Vec<AudibleSpan<'_>> {
        let mut spans = Vec::new();
        for clip in self.video_clips().filter(|c| c.has_audible_sound()) {
            spans.push(AudibleSpan {
                label: format!("video track {} clip {}", clip.track, clip.index),
                source: &clip.source,
                source_in: clip.source_in,
                start: self.fps.seconds(clip.start_frames),
                duration: self.fps.seconds(clip.duration_frames),
                speed: clip.speed,
                mix: &clip.audio,
                role: self.video[clip.track].role,
            });
        }
        for clip in self.audio_clips().filter(|c| c.mix.audible()) {
            spans.push(AudibleSpan {
                label: format!("audio track {} clip {}", clip.track, clip.index),
                source: &clip.source,
                source_in: clip.source_in,
                start: self.fps.seconds(clip.start_frames),
                duration: self.fps.seconds(clip.duration_frames),
                speed: clip.speed,
                mix: &clip.mix,
                role: self.audio[clip.track].role,
            });
        }
        spans
    }
}

pub fn load_and_validate(path: &Path) -> Result<ValidatedEdit> {
    let value: Value = io::read_json(path)?;
    let edit = parse(value)?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let mut validated = validate(&edit, base)?;
    validated.inputs.push(media::canonical_file(path)?);
    Ok(validated)
}

/// Parse an edit document, explaining version mismatches before field errors.
pub fn parse(value: Value) -> Result<Edit> {
    match value.get("version").and_then(Value::as_u64) {
        Some(2) => {}
        Some(1) => bail!(
            "edit version 1 is no longer supported; convert it with `skywrite edit migrate OLD.json --output NEW.json`"
        ),
        None if value.get("clips").is_some() => bail!(
            "this looks like a version 1 edit (top-level `clips`); convert it with `skywrite edit migrate OLD.json --output NEW.json`"
        ),
        None => bail!("edit requires \"version\": {EDIT_VERSION}"),
        Some(other) => bail!("unsupported edit version {other}; expected {EDIT_VERSION}"),
    }
    serde_json::from_value(value).context("invalid edit structure (run `skywrite schema`)")
}

pub fn validate(edit: &Edit, base_dir: &Path) -> Result<ValidatedEdit> {
    ensure!(
        edit.version == EDIT_VERSION,
        "unsupported edit version {}; expected {EDIT_VERSION}",
        edit.version
    );
    ensure!(
        !edit.name.trim().is_empty() && edit.name.len() <= 512 && xml_text(&edit.name),
        "edit name must be 1–512 UTF-8 bytes without XML control characters"
    );
    ensure!(
        (2..=8192).contains(&edit.width)
            && edit.width.is_multiple_of(2)
            && (2..=8192).contains(&edit.height)
            && edit.height.is_multiple_of(2),
        "output width and height must be even integers between 2 and 8192"
    );
    ensure!(
        edit.fps.num > 0
            && edit.fps.den > 0
            && edit.fps.num <= 1_000_000
            && edit.fps.den <= 1_000_000
            && (1.0..=240.0).contains(&edit.fps.value()),
        "fps must be a positive rational between 1 and 240 frames/second with numerator/denominator <= 1000000"
    );
    ensure!(
        edit.video.len() <= MAX_VIDEO_TRACKS,
        "edit may contain at most {MAX_VIDEO_TRACKS} video tracks"
    );
    ensure!(
        edit.audio.len() <= MAX_AUDIO_TRACKS,
        "edit may contain at most {MAX_AUDIO_TRACKS} audio tracks"
    );
    let video_count: usize = edit.video.iter().map(|t| t.clips.len()).sum();
    let audio_count: usize = edit.audio.iter().map(|t| t.clips.len()).sum();
    ensure!(
        video_count <= MAX_VIDEO_CLIPS,
        "edit may contain at most {MAX_VIDEO_CLIPS} video clips"
    );
    ensure!(
        audio_count <= MAX_AUDIO_CLIPS,
        "edit may contain at most {MAX_AUDIO_CLIPS} audio clips"
    );
    ensure!(
        video_count + audio_count > 0 || edit.duration.is_some(),
        "edit must contain at least one clip or an explicit duration"
    );
    if let Some(limit) = edit.master.limiter_db {
        ensure!(
            limit.is_finite() && (-24.0..=0.0).contains(&limit),
            "master.limiter_db must be between -24 and 0 dBFS"
        );
    }
    let mut warnings = Vec::new();
    let mut probes = HashMap::<PathBuf, media::MediaInfo>::new();

    // Per-source settings: canonical path -> (lut index, declaration).
    let mut luts = Vec::<ValidatedLut>::new();
    let mut settings = HashMap::<PathBuf, (Option<usize>, Option<ColorDeclaration>)>::new();
    let mut sources = Vec::new();
    ensure!(
        edit.sources.len() <= 256,
        "edit may contain at most 256 source settings"
    );
    for (index, entry) in edit.sources.iter().enumerate() {
        let path = resolve(&entry.path, base_dir).with_context(|| format!("sources[{index}]"))?;
        ensure!(
            !settings.contains_key(&path),
            "sources[{index}]: {} is listed more than once",
            path.display()
        );
        let lut = match &entry.lut {
            Some(lut) => {
                let lut =
                    resolve(lut, base_dir).with_context(|| format!("sources[{index}].lut"))?;
                let existing = luts.iter().position(|l| l.path == lut);
                Some(match existing {
                    Some(position) => position,
                    None => {
                        luts.push(load_lut(&lut).with_context(|| format!("sources[{index}].lut"))?);
                        ensure!(
                            luts.iter().map(|l| l.text.len() as u64).sum::<u64>() <= MAX_LUT_BYTES,
                            "LUT files are limited to 32 MiB combined"
                        );
                        luts.len() - 1
                    }
                })
            }
            None => None,
        };
        if let Some(color) = &entry.color {
            for (field, value) in [
                ("transfer", &color.transfer),
                ("primaries", &color.primaries),
                ("matrix", &color.matrix),
                ("note", &color.note),
            ] {
                if let Some(value) = value {
                    ensure!(
                        !value.trim().is_empty() && value.len() <= 256 && xml_text(value),
                        "sources[{index}].color.{field} must be 1–256 bytes of plain text"
                    );
                }
            }
        }
        settings.insert(path.clone(), (lut, entry.color.clone()));
        sources.push(ValidatedSource {
            path,
            color: entry.color.clone(),
            lut: lut.map(|i| luts[i].path.clone()),
        });
    }

    let mut used_sources = std::collections::HashSet::new();
    let mut end_frames = 0u64;
    let mut video = Vec::with_capacity(edit.video.len());
    for (track_index, track) in edit.video.iter().enumerate() {
        let name = track_name(track.name.as_deref(), "V", track_index)?;
        let mut clips = Vec::with_capacity(track.clips.len());
        for (index, clip) in track.clips.iter().enumerate() {
            let label = format!("video track {track_index} clip {index}");
            validate_numbers(clip.source_in, clip.duration, clip.speed).context(label.clone())?;
            ensure!(
                clip.opacity.is_finite() && (0.0..=1.0).contains(&clip.opacity),
                "{label}: opacity must be between 0 and 1"
            );
            let (start_frames, duration_frames) =
                place(edit.fps, clip.start, clip.duration, &label, &mut warnings)?;
            let duration = edit.fps.seconds(duration_frames);
            validate_fades(
                clip.fade_in,
                clip.fade_out,
                duration,
                &format!("{label} picture"),
            )?;
            let audio = validate_mix(&clip.audio, duration, &format!("{label} audio"))?;
            let path = resolve(&clip.source, base_dir).context(label.clone())?;
            let info = probe_cached(&path, &mut probes)?;
            ensure!(
                info.has_video(),
                "{label}: {} has no video/image stream; put sound-only media on an audio track",
                path.display()
            );
            let kind = if is_still(&info) {
                MediaKind::Image
            } else {
                MediaKind::Video
            };
            if kind == MediaKind::Image {
                ensure!(
                    clip.source_in == 0.0 && clip.speed == 1.0,
                    "{label}: still images require source_in 0 and speed 1"
                );
                if clip.audio != Mix::default() {
                    warnings.push(format!(
                        "{label}: still images have no sound; its audio settings are ignored"
                    ));
                }
            } else {
                check_range(clip.source_in, duration, clip.speed, &info, &label)?;
                if !info.has_audio() && clip.audio != Mix::default() && clip.audio.volume > 0.0 {
                    warnings.push(format!(
                        "{label}: source has no audio stream; its audio settings have no effect"
                    ));
                }
            }
            let (lut, color) = settings.get(&path).cloned().unwrap_or((None, None));
            if lut.is_none()
                && let Some(transfer) = color.as_ref().and_then(|c| c.transfer.as_deref())
                && looks_logarithmic(transfer)
            {
                warnings.push(format!(
                    "{label}: {} is declared as {transfer} without a LUT; rendered output will look flat. Add sources[].lut or grade in the NLE",
                    path.display()
                ));
            }
            used_sources.insert(path.clone());
            clips.push(ValidatedVideoClip {
                track: track_index,
                index,
                source: path,
                kind,
                source_in: clip.source_in,
                start_frames,
                duration_frames,
                speed: clip.speed,
                opacity: clip.opacity,
                fade_in: clip.fade_in,
                fade_out: clip.fade_out,
                audio,
                media: info,
                lut,
            });
        }
        clips.sort_by_key(|c| c.start_frames);
        check_overlaps(
            clips
                .iter()
                .map(|c| (c.start_frames, c.end_frames(), c.index)),
            &name,
            &mut warnings,
        )?;
        if let Some(last) = clips.iter().map(ValidatedVideoClip::end_frames).max() {
            end_frames = end_frames.max(last);
        }
        video.push(ValidatedTrackInfo {
            name,
            role: track.role,
            clips,
        });
    }

    let mut audio = Vec::with_capacity(edit.audio.len());
    for (track_index, track) in edit.audio.iter().enumerate() {
        let name = track_name(track.name.as_deref(), "A", track_index)?;
        let mut clips = Vec::with_capacity(track.clips.len());
        for (index, clip) in track.clips.iter().enumerate() {
            let label = format!("audio track {track_index} clip {index}");
            validate_numbers(clip.source_in, clip.duration, clip.speed).context(label.clone())?;
            let (start_frames, duration_frames) =
                place(edit.fps, clip.start, clip.duration, &label, &mut warnings)?;
            let duration = edit.fps.seconds(duration_frames);
            let mix = validate_mix(
                &Mix {
                    volume: clip.volume,
                    volume_keyframes: clip.volume_keyframes.clone(),
                    fade_in: clip.fade_in,
                    fade_out: clip.fade_out,
                },
                duration,
                &label,
            )?;
            let path = resolve(&clip.source, base_dir).context(label.clone())?;
            let info = probe_cached(&path, &mut probes)?;
            ensure!(
                info.has_audio(),
                "{label}: {} has no audio stream",
                path.display()
            );
            check_range(clip.source_in, duration, clip.speed, &info, &label)?;
            used_sources.insert(path.clone());
            clips.push(ValidatedAudioClip {
                track: track_index,
                index,
                source: path,
                source_in: clip.source_in,
                start_frames,
                duration_frames,
                speed: clip.speed,
                mix,
                media: info,
            });
        }
        clips.sort_by_key(|c| c.start_frames);
        check_overlaps(
            clips
                .iter()
                .map(|c| (c.start_frames, c.end_frames(), c.index)),
            &name,
            &mut warnings,
        )?;
        if let Some(last) = clips.iter().map(ValidatedAudioClip::end_frames).max() {
            end_frames = end_frames.max(last);
        }
        audio.push(ValidatedTrackInfo {
            name,
            role: track.role,
            clips,
        });
    }

    for source in &sources {
        if !used_sources.contains(&source.path) {
            warnings.push(format!(
                "sources entry {} is not used by any clip",
                source.path.display()
            ));
        }
    }
    let duration_frames = match edit.duration {
        Some(duration) => {
            ensure!(
                duration.is_finite() && duration > 0.0 && duration <= MAX_TIMELINE_SECONDS,
                "duration must be finite and in (0, 86400] seconds"
            );
            let frames = quantize(edit.fps, duration, "timeline duration", &mut warnings)?;
            ensure!(
                frames >= end_frames,
                "timeline duration {:.6}s is shorter than the last clip end {:.6}s",
                edit.fps.seconds(frames),
                edit.fps.seconds(end_frames)
            );
            frames
        }
        None => end_frames,
    };
    ensure!(
        duration_frames > 0,
        "timeline duration rounds to zero frames"
    );
    ensure!(
        edit.fps.seconds(duration_frames) <= MAX_TIMELINE_SECONDS,
        "timeline is limited to 24 hours"
    );
    let total = edit.fps.seconds(duration_frames);
    let derive = |paths: &[PathBuf]| transcript_cues(paths, &video, &audio, edit.fps);
    let captions = captions::load_tracks(&edit.captions, base_dir, total, &derive)?;
    captions::validate_chapters(&edit.chapters, total)?;
    Ok(ValidatedEdit {
        name: edit.name.clone(),
        width: edit.width,
        height: edit.height,
        fps: edit.fps,
        duration_frames,
        video,
        audio,
        master: edit.master.clone(),
        luts,
        sources,
        warnings,
        captions,
        chapters: edit.chapters.clone(),
        inputs: Vec::new(),
    })
}

/// Convert a version 1 edit document to version 2 without probing media.
///
/// Version 1 primary clips played back to back; each is placed on video
/// track V1 at the frame where it used to start. Gaps become empty timeline,
/// with an explicit `duration` preserving a trailing gap. Overlay audio clips
/// are packed onto the fewest audio tracks without overlap.
pub fn migrate_v1(value: &Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct V1 {
        #[serde(default)]
        version: Option<u32>,
        #[serde(default = "default_name")]
        name: String,
        #[serde(default = "default_width")]
        width: u32,
        #[serde(default = "default_height")]
        height: u32,
        #[serde(default)]
        fps: Rational,
        clips: Vec<V1Clip>,
        #[serde(default)]
        audio: Vec<V1Audio>,
        #[serde(default)]
        captions: Vec<CaptionTrack>,
        #[serde(default)]
        chapters: Vec<Chapter>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct V1Clip {
        #[serde(default)]
        source: Option<PathBuf>,
        #[serde(default)]
        source_in: f64,
        duration: f64,
        #[serde(default = "one")]
        speed: f64,
        #[serde(default = "one")]
        volume: f64,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct V1Audio {
        source: PathBuf,
        #[serde(default)]
        source_in: f64,
        #[serde(default)]
        offset: f64,
        duration: f64,
        #[serde(default = "one")]
        speed: f64,
        #[serde(default = "one")]
        volume: f64,
    }
    if let Some(version) = value.get("version").and_then(Value::as_u64)
        && version != 1
    {
        bail!(
            "this edit is already version {version}; migrate only converts version 1 edits (validate it with skywrite edit validate)"
        );
    }
    let old: V1 = serde_json::from_value(value.clone()).context("not a valid version 1 edit")?;
    ensure!(
        old.version.unwrap_or(1) == 1,
        "migrate expects a version 1 edit; this one is version {}",
        old.version.unwrap_or(1)
    );
    ensure!(
        old.fps.num > 0 && old.fps.den > 0,
        "fps must have a positive numerator and denominator"
    );
    let fps = old.fps;
    let seconds = |frames: u64| -> Value { json!(round_seconds(fps.seconds(frames))) };
    let mut cursor = 0u64;
    let mut v1_clips = Vec::new();
    let still = |path: &Path| {
        path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "bmp" | "tif" | "tiff" | "gif"
            )
        })
    };
    for (index, clip) in old.clips.iter().enumerate() {
        // Version 1 rejected clips that round to zero frames; so does migration.
        let frames = fps.frames(clip.duration);
        ensure!(
            frames > 0,
            "clip {index} duration rounds to zero frames; version 1 rejected it too"
        );
        if let Some(source) = &clip.source {
            let mut item = json!({"start":seconds(cursor),"source":source,"source_in":clip.source_in,"duration":seconds(frames)});
            if clip.speed != 1.0 {
                item["speed"] = json!(clip.speed);
            }
            if clip.volume != 1.0 && !still(source) {
                item["audio"] = json!({"volume":clip.volume});
            }
            v1_clips.push(item);
        }
        cursor += frames;
    }
    // Greedy interval packing keeps overlapping overlays on separate tracks.
    let mut tracks: Vec<(u64, Vec<Value>)> = Vec::new();
    let mut order: Vec<&V1Audio> = old.audio.iter().collect();
    order.sort_by_key(|a| fps.frames(a.offset));
    for (index, clip) in order.into_iter().enumerate() {
        let start = fps.frames(clip.offset);
        let frames = fps.frames(clip.duration);
        ensure!(
            frames > 0,
            "audio clip {index} duration rounds to zero frames; version 1 rejected it too"
        );
        let mut item = json!({"start":seconds(start),"source":clip.source,"source_in":clip.source_in,"duration":seconds(frames)});
        if clip.speed != 1.0 {
            item["speed"] = json!(clip.speed);
        }
        if clip.volume != 1.0 {
            item["volume"] = json!(clip.volume);
        }
        match tracks.iter_mut().find(|(end, _)| *end <= start) {
            Some(track) => {
                track.0 = start + frames;
                track.1.push(item);
            }
            None => tracks.push((start + frames, vec![item])),
        }
    }
    let mut out = json!({
        "version": EDIT_VERSION,
        "name": old.name,
        "width": old.width,
        "height": old.height,
        "fps": old.fps,
        "video": [{"name":"V1","clips":v1_clips}],
        "audio": tracks.into_iter().enumerate().map(|(i,(_,clips))| json!({"name":format!("A{}", i+1),"clips":clips})).collect::<Vec<_>>(),
    });
    let last_clip_end = out["video"][0]["clips"]
        .as_array()
        .and_then(|c| c.last())
        .map(|c| {
            fps.frames(c["start"].as_f64().unwrap_or(0.0))
                + fps.frames(c["duration"].as_f64().unwrap_or(0.0))
        })
        .unwrap_or(0);
    if cursor > last_clip_end {
        out["duration"] = seconds(cursor);
    }
    if !old.captions.is_empty() {
        out["captions"] = serde_json::to_value(&old.captions)?;
    }
    if !old.chapters.is_empty() {
        out["chapters"] = serde_json::to_value(&old.chapters)?;
    }
    Ok(out)
}

/// Map source transcripts through the edit: a word is captioned where its
/// midpoint is audible in a clip of its source, at its output-timeline time.
fn transcript_cues(
    paths: &[PathBuf],
    video: &[ValidatedTrackInfo<ValidatedVideoClip>],
    audio: &[ValidatedTrackInfo<ValidatedAudioClip>],
    fps: Rational,
) -> Result<(Vec<captions::Cue>, Vec<PathBuf>)> {
    let mut by_source = HashMap::new();
    let mut inputs = Vec::new();
    for path in paths {
        let transcript = crate::transcript::Transcript::load(path)?;
        let base = path.parent().unwrap_or(Path::new("."));
        let source = if transcript.source.is_absolute() {
            transcript.source.clone()
        } else {
            base.join(&transcript.source)
        };
        let source = media::canonical_file(&source)
            .with_context(|| format!("transcript {} source", path.display()))?;
        inputs.push(source.clone());
        ensure!(
            by_source.insert(source.clone(), transcript).is_none(),
            "two caption transcripts describe {}",
            source.display()
        );
    }
    let spans = video
        .iter()
        .flat_map(|t| t.clips.iter())
        .filter(|c| c.has_audible_sound())
        .map(|c| {
            (
                &c.source,
                c.source_in,
                c.start_frames,
                c.duration_frames,
                c.speed,
                &c.audio,
            )
        })
        .chain(
            audio
                .iter()
                .flat_map(|t| t.clips.iter())
                .filter(|c| c.mix.audible())
                .map(|c| {
                    (
                        &c.source,
                        c.source_in,
                        c.start_frames,
                        c.duration_frames,
                        c.speed,
                        &c.mix,
                    )
                }),
        );
    // Words keep the index of the clip they were heard in, so cues break at cuts.
    let mut words: Vec<(usize, crate::transcript::Word)> = Vec::new();
    for (span, (source, source_in, start_frames, duration_frames, speed, mix)) in spans.enumerate()
    {
        let Some(transcript) = by_source.get(source) else {
            continue;
        };
        let start = fps.seconds(start_frames);
        let duration = fps.seconds(duration_frames);
        let source_end = source_in + duration * speed;
        for word in &transcript.words {
            let middle = (word.start + word.end) / 2.0;
            if middle < source_in || middle >= source_end {
                continue;
            }
            let at = |t: f64| start + (t.clamp(source_in, source_end) - source_in) / speed;
            let (t0, t1) = (at(word.start), at(word.end));
            if mix.gain((t0 + t1) / 2.0 - start, duration) <= 0.0 {
                continue;
            }
            let mut mapped = word.clone();
            mapped.start = t0;
            mapped.end = t1.max(t0 + 0.001);
            mapped.audio_level_dbfs = None;
            words.push((span, mapped));
        }
    }
    words.sort_by(|a, b| a.1.start.total_cmp(&b.1.start));
    // The same words heard from two sources at once are captioned once.
    let normal = |t: &str| {
        t.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let mut unique: Vec<(usize, crate::transcript::Word)> = Vec::with_capacity(words.len());
    for (span, word) in words {
        let duplicate = unique.iter().rev().take(8).any(|(_, kept)| {
            let overlap = kept.end.min(word.end) - kept.start.max(word.start);
            normal(&kept.text) == normal(&word.text)
                && overlap > 0.5 * (word.end - word.start).min(kept.end - kept.start)
        });
        if !duplicate {
            unique.push((span, word));
        }
    }
    let mut cues = Vec::new();
    let mut flush = |group: &mut Vec<crate::transcript::Word>| -> Result<()> {
        if group.is_empty() {
            return Ok(());
        }
        let words = std::mem::take(group);
        let timeline = crate::transcript::Transcript {
            version: 1,
            source: paths[0].clone(),
            duration: words.iter().map(|w| w.end).fold(0.0, f64::max),
            text: words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            words,
            model: None,
            usage: None,
        };
        cues.extend(captions::words(
            &timeline,
            &captions::ConvertOptions::default(),
        )?);
        Ok(())
    };
    let mut group = Vec::new();
    let mut current = None;
    for (span, word) in unique {
        if current.is_some_and(|c| c != span) {
            flush(&mut group)?;
        }
        current = Some(span);
        group.push(word);
    }
    flush(&mut group)?;
    Ok((cues, inputs))
}

/// Twelve decimals keep frame positions exact after rounding back to frames.
fn round_seconds(value: f64) -> f64 {
    (value * 1e12).round() / 1e12
}

fn looks_logarithmic(transfer: &str) -> bool {
    let t = transfer.to_ascii_lowercase();
    t.contains("log")
        && !t.contains("logarithmic-100")
        && !t.contains("log100")
        && !t.contains("log316")
}

fn track_name(name: Option<&str>, prefix: &str, index: usize) -> Result<String> {
    match name {
        Some(name) => {
            ensure!(
                !name.trim().is_empty() && name.len() <= 64 && xml_text(name),
                "track names must be 1–64 bytes of plain text"
            );
            Ok(name.to_owned())
        }
        None => Ok(format!("{prefix}{}", index + 1)),
    }
}

fn check_overlaps(
    clips: impl Iterator<Item = (u64, u64, usize)>,
    track: &str,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let mut previous: Option<(u64, usize)> = None;
    for (start, end, index) in clips {
        if let Some((previous_end, previous_index)) = previous {
            ensure!(
                start >= previous_end,
                "track {track}: clip {index} overlaps clip {previous_index}{}; put overlapping clips on separate tracks",
                if previous_end - start == 1 {
                    " by one frame (often rounding: compute each start from the previous clip's frame-rounded end)"
                } else {
                    ""
                }
            );
            // Starts and durations round independently, so float times can
            // leave a single black/silent frame between intended neighbors.
            if start - previous_end == 1 {
                warnings.push(format!(
                    "track {track}: one-frame gap between clip {previous_index} and clip {index}; if they should touch, set the start to the previous clip's frame-rounded end"
                ));
            }
        }
        previous = Some((end, index));
    }
    Ok(())
}

fn place(
    fps: Rational,
    start: f64,
    duration: f64,
    label: &str,
    warnings: &mut Vec<String>,
) -> Result<(u64, u64)> {
    ensure!(
        start.is_finite() && (0.0..=MAX_TIMELINE_SECONDS).contains(&start),
        "{label}: start must be finite, nonnegative and at most 86400 seconds"
    );
    let start_frames = fps.frames(start);
    if (fps.seconds(start_frames) - start).abs() > 1e-7 {
        warnings.push(format!(
            "{label} start rounded to output frame {start_frames}"
        ));
    }
    let duration_frames = quantize(fps, duration, &format!("{label} duration"), warnings)?;
    Ok((start_frames, duration_frames))
}

fn validate_fades(fade_in: f64, fade_out: f64, duration: f64, label: &str) -> Result<()> {
    ensure!(
        fade_in.is_finite() && fade_in >= 0.0 && fade_out.is_finite() && fade_out >= 0.0,
        "{label}: fades must be finite and nonnegative"
    );
    ensure!(
        fade_in + fade_out <= duration + 1e-9,
        "{label}: fade_in + fade_out exceed the clip duration"
    );
    Ok(())
}

fn validate_mix(mix: &Mix, duration: f64, label: &str) -> Result<ValidatedMix> {
    ensure!(
        mix.volume.is_finite() && (0.0..=4.0).contains(&mix.volume),
        "{label}: volume must be finite and between 0 and 4 (linear gain)"
    );
    validate_fades(mix.fade_in, mix.fade_out, duration, label)?;
    ensure!(
        mix.volume_keyframes.len() <= MAX_KEYFRAMES,
        "{label}: at most {MAX_KEYFRAMES} volume keyframes"
    );
    let mut previous = -1.0f64;
    for keyframe in &mix.volume_keyframes {
        ensure!(
            keyframe.time.is_finite() && keyframe.time >= 0.0 && keyframe.time <= duration + 1e-9,
            "{label}: keyframe time {} is outside the clip (0..{duration:.6}s)",
            keyframe.time
        );
        ensure!(
            keyframe.time > previous,
            "{label}: keyframe times must strictly increase"
        );
        ensure!(
            keyframe.volume.is_finite() && (0.0..=4.0).contains(&keyframe.volume),
            "{label}: keyframe volume must be between 0 and 4 (linear gain)"
        );
        previous = keyframe.time;
    }
    Ok(ValidatedMix {
        volume: mix.volume,
        keyframes: mix.volume_keyframes.clone(),
        fade_in: mix.fade_in,
        fade_out: mix.fade_out,
    })
}

fn load_lut(path: &Path) -> Result<ValidatedLut> {
    ensure!(
        path.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("cube")),
        "LUTs must be .cube files: {}",
        path.display()
    );
    let metadata = std::fs::metadata(path)?;
    ensure!(
        metadata.len() <= MAX_LUT_BYTES,
        "LUT {} exceeds 32 MiB",
        path.display()
    );
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_LUT_BYTES,
        "LUT {} exceeds 32 MiB",
        path.display()
    );
    let text = String::from_utf8(bytes).context("LUT must be UTF-8 text")?;
    ensure!(!text.contains('\0'), "LUT contains NUL bytes");
    let mut size = None;
    let mut rows = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("LUT_3D_SIZE") {
            size = Some((
                "3d",
                rest.trim()
                    .parse::<usize>()
                    .context("invalid LUT_3D_SIZE")?,
            ));
        } else if let Some(rest) = line.strip_prefix("LUT_1D_SIZE") {
            size = Some((
                "1d",
                rest.trim()
                    .parse::<usize>()
                    .context("invalid LUT_1D_SIZE")?,
            ));
        } else if line
            .split_whitespace()
            .next()
            .is_some_and(|first| first.parse::<f64>().is_ok())
        {
            ensure!(
                line.split_whitespace().count() == 3
                    && line
                        .split_whitespace()
                        .all(|v| v.parse::<f64>().is_ok_and(f64::is_finite)),
                "LUT rows must contain three finite numbers"
            );
            rows += 1;
        }
    }
    let (kind, size) = size.context("LUT has no LUT_3D_SIZE or LUT_1D_SIZE line")?;
    let expected = if kind == "3d" {
        ensure!((2..=256).contains(&size), "LUT_3D_SIZE must be 2..256");
        size * size * size
    } else {
        ensure!((2..=65536).contains(&size), "LUT_1D_SIZE must be 2..65536");
        size
    };
    ensure!(
        rows == expected,
        "LUT {} declares {expected} rows but contains {rows}",
        path.display()
    );
    Ok(ValidatedLut {
        path: path.to_owned(),
        text,
    })
}

fn xml_text(value: &str) -> bool {
    value.chars().all(|c| {
        c == '\t' || c == '\n' || c == '\r' || (c >= ' ' && c != '\u{fffe}' && c != '\u{ffff}')
    })
}
fn validate_numbers(source_in: f64, duration: f64, speed: f64) -> Result<()> {
    ensure!(
        source_in.is_finite() && source_in >= 0.0,
        "source_in must be finite and nonnegative"
    );
    ensure!(
        duration.is_finite() && duration > 0.0 && duration <= MAX_TIMELINE_SECONDS,
        "duration must be finite and in (0, 86400] output seconds"
    );
    ensure!(
        speed.is_finite() && (0.125..=8.0).contains(&speed),
        "speed must be finite and between 0.125 and 8 source seconds per output second"
    );
    Ok(())
}
fn quantize(fps: Rational, duration: f64, label: &str, warnings: &mut Vec<String>) -> Result<u64> {
    let frames = fps.frames(duration);
    ensure!(frames > 0, "{label} rounds to zero output frames");
    if (fps.seconds(frames) - duration).abs() > 1e-7 {
        warnings.push(format!("{label} rounded to {frames} output frames"));
    }
    Ok(frames)
}
fn resolve(path: &Path, base: &Path) -> Result<PathBuf> {
    ensure!(!path.as_os_str().is_empty(), "source path cannot be empty");
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    let path = media::canonical_file(&path)?;
    let text = path
        .to_str()
        .context("source paths must be valid UTF-8 for interchange")?;
    ensure!(
        xml_text(text),
        "source path contains XML control characters"
    );
    Ok(path)
}
fn probe_cached(
    path: &Path,
    cache: &mut HashMap<PathBuf, media::MediaInfo>,
) -> Result<media::MediaInfo> {
    if let Some(info) = cache.get(path) {
        return Ok(info.clone());
    }
    let info = media::probe_for_timing(path)?;
    cache.insert(path.to_owned(), info.clone());
    Ok(info)
}
fn check_range(
    start: f64,
    duration: f64,
    speed: f64,
    info: &media::MediaInfo,
    label: &str,
) -> Result<()> {
    if !info.duration.is_finite() || info.duration <= 0.0 {
        bail!("{label}: source does not have a known positive duration");
    }
    let end = start + duration * speed;
    ensure!(
        end.is_finite() && end <= info.duration + 0.002,
        "{label}: source range ends at {end:.6}s, beyond {} duration {:.6}s",
        info.path.display(),
        info.duration
    );
    Ok(())
}
fn is_still(info: &media::MediaInfo) -> bool {
    info.format_name.split(',').any(|f| {
        matches!(
            f,
            "image2" | "png_pipe" | "jpeg_pipe" | "bmp_pipe" | "tiff_pipe" | "webp_pipe"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rational_ntsc_is_exact() {
        let fps = Rational {
            num: 30000,
            den: 1001,
        };
        assert_eq!(fps.fcpxml(1), "1001/30000s");
        assert_eq!(fps.fcpxml(30000), "1001s");
        assert_eq!(fps.frames(fps.seconds(12345)), 12345);
        assert_eq!(fps.samples_48k(30000), 48_048_000);
    }
    #[test]
    fn parse_explains_versions() {
        let v1 = parse(json!({"clips":[{"duration":1}]}))
            .unwrap_err()
            .to_string();
        assert!(v1.contains("edit migrate"), "{v1}");
        let v1 = parse(json!({"version":1,"clips":[]}))
            .unwrap_err()
            .to_string();
        assert!(v1.contains("edit migrate"), "{v1}");
        assert!(parse(json!({"version":2,"video":[],"transition":"x"})).is_err());
        assert!(parse(json!({"version":2,"video":[]})).is_ok());
    }
    #[test]
    fn rejects_empty_edits() {
        let edit = parse(json!({"version":2})).unwrap();
        assert!(validate(&edit, Path::new(".")).is_err());
    }
    #[test]
    fn mix_curve_combines_volume_keyframes_and_fades() {
        let mix = ValidatedMix {
            volume: 0.5,
            keyframes: vec![
                Keyframe {
                    time: 1.0,
                    volume: 1.0,
                },
                Keyframe {
                    time: 3.0,
                    volume: 0.0,
                },
            ],
            fade_in: 0.5,
            fade_out: 0.0,
        };
        assert_eq!(mix.gain(0.0, 4.0), 0.0);
        assert!((mix.gain(0.25, 4.0) - 0.25).abs() < 1e-12);
        assert!((mix.gain(2.0, 4.0) - 0.25).abs() < 1e-12);
        assert_eq!(mix.gain(3.5, 4.0), 0.0);
        let points = mix.breakpoints(4.0);
        let times: Vec<f64> = points.iter().map(|k| k.time).collect();
        for expected in [0.0, 0.5, 1.0, 3.0, 4.0] {
            assert!(
                times.iter().any(|t| (t - expected).abs() < 1e-9),
                "{times:?}"
            );
        }
        assert!(times.windows(2).all(|w| w[0] < w[1]));
        assert!(
            points
                .iter()
                .all(|k| (k.volume - mix.gain(k.time, 4.0)).abs() < 1e-12)
        );
        assert!(mix.audible());
        // A fade over a ramp is curved; exported points follow it.
        let curved = ValidatedMix {
            volume: 1.0,
            keyframes: vec![
                Keyframe {
                    time: 0.0,
                    volume: 0.0,
                },
                Keyframe {
                    time: 4.0,
                    volume: 1.0,
                },
            ],
            fade_in: 0.0,
            fade_out: 4.0,
        };
        let points = curved.breakpoints(4.0);
        let peak = points.iter().map(|k| k.volume).fold(0.0, f64::max);
        assert!((peak - 0.25).abs() < 0.01, "{peak}");
    }
    #[test]
    fn migrate_places_sequential_clips_and_packs_audio() {
        let v2 = migrate_v1(&json!({"fps":{"num":30,"den":1},"clips":[
            {"source":"a.mp4","source_in":2,"duration":1.01,"volume":0},
            {"duration":0.5},
            {"source":"b.mp4","duration":2,"speed":1.5},
            {"duration":1}
        ],"audio":[
            {"source":"r.wav","offset":0,"duration":2},
            {"source":"m.wav","offset":1,"duration":2,"volume":0.5},
            {"source":"r.wav","offset":2,"duration":1}
        ]}))
        .unwrap();
        let clips = v2["video"][0]["clips"].as_array().unwrap();
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0]["audio"]["volume"], 0.0);
        assert_eq!(clips[1]["start"].as_f64().unwrap(), 1.5);
        assert_eq!(clips[1]["speed"], 1.5);
        assert_eq!(v2["duration"].as_f64().unwrap(), 4.5);
        let audio = v2["audio"].as_array().unwrap();
        assert_eq!(audio.len(), 2);
        assert_eq!(audio[0]["clips"].as_array().unwrap().len(), 2);
        assert_eq!(audio[1]["clips"][0]["volume"], 0.5);
        assert!(parse(v2).is_ok());
        assert!(migrate_v1(&json!({"version":2,"clips":[]})).is_err());
        // Stills drop v1's meaningless volume; zero-frame clips stay invalid.
        let stills =
            migrate_v1(&json!({"clips":[{"source":"t.PNG","duration":1,"volume":0}]})).unwrap();
        assert!(stills["video"][0]["clips"][0].get("audio").is_none());
        assert!(migrate_v1(&json!({"clips":[{"source":"a.mp4","duration":0.01}]})).is_err());
    }
    #[test]
    fn overlaps_and_bad_mixes_are_rejected() {
        let mut warnings = Vec::new();
        assert!(check_overlaps([(0, 10, 0), (10, 20, 1)].into_iter(), "V1", &mut warnings).is_ok());
        assert!(warnings.is_empty());
        let error = check_overlaps([(0, 10, 0), (9, 20, 1)].into_iter(), "V1", &mut warnings)
            .unwrap_err()
            .to_string();
        assert!(error.contains("one frame"), "{error}");
        assert!(check_overlaps([(0, 10, 0), (11, 20, 1)].into_iter(), "V1", &mut warnings).is_ok());
        assert!(warnings[0].contains("one-frame gap"));
        let bad = Mix {
            volume_keyframes: vec![
                Keyframe {
                    time: 1.0,
                    volume: 1.0,
                },
                Keyframe {
                    time: 1.0,
                    volume: 0.5,
                },
            ],
            ..Mix::default()
        };
        assert!(validate_mix(&bad, 2.0, "x").is_err());
        let long_fades = Mix {
            fade_in: 1.5,
            fade_out: 1.0,
            ..Mix::default()
        };
        assert!(validate_mix(&long_fades, 2.0, "x").is_err());
    }
    #[test]
    fn lut_parser_checks_row_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id.cube");
        let mut text = String::from("TITLE \"identity\"\nLUT_3D_SIZE 2\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    text.push_str(&format!("{r} {g} {b}\n"));
                }
            }
        }
        std::fs::write(&path, &text).unwrap();
        assert!(load_lut(&path).is_ok());
        std::fs::write(&path, text.replace("1 1 1\n", "")).unwrap();
        assert!(load_lut(&path).is_err());
        std::fs::write(dir.path().join("x.txt"), "LUT_3D_SIZE 2").unwrap();
        assert!(load_lut(&dir.path().join("x.txt")).is_err());
    }
}
