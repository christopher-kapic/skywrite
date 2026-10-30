//! The small, validated edit format shared by every output backend.
//!
//! All edit times are seconds measured from the first presented sample. `duration`
//! is output duration, while `speed` is source seconds per output second.
use crate::{io, media};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

fn one() -> f64 {
    1.0
}
fn default_version() -> u32 {
    1
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default)]
    pub fps: Rational,
    pub clips: Vec<Clip>,
    #[serde(default)]
    pub audio: Vec<AudioClip>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clip {
    /// Omit source or set null for a black gap with silent audio.
    #[serde(default)]
    pub source: Option<PathBuf>,
    #[serde(default)]
    pub source_in: f64,
    pub duration: f64,
    #[serde(default = "one")]
    pub speed: f64,
    /// Linear audio gain; zero mutes the source audio.
    #[serde(default = "one")]
    pub volume: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioClip {
    pub source: PathBuf,
    #[serde(default)]
    pub source_in: f64,
    #[serde(default)]
    pub offset: f64,
    pub duration: f64,
    #[serde(default = "one")]
    pub speed: f64,
    #[serde(default = "one")]
    pub volume: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Video,
    Image,
    Gap,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidatedClip {
    pub source: Option<PathBuf>,
    pub kind: MediaKind,
    pub source_in: f64,
    pub offset_frames: u64,
    pub duration_frames: u64,
    pub speed: f64,
    pub volume: f64,
    pub media: Option<media::MediaInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidatedAudio {
    pub source: PathBuf,
    pub source_in: f64,
    pub offset_frames: u64,
    pub duration_frames: u64,
    pub speed: f64,
    pub volume: f64,
    pub media: media::MediaInfo,
}

/// Constructed only by the validator; exporters cannot accidentally trust raw JSON.
#[derive(Debug, Clone, Serialize)]
pub struct ValidatedEdit {
    pub(crate) name: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: Rational,
    pub(crate) duration_frames: u64,
    pub(crate) clips: Vec<ValidatedClip>,
    pub(crate) audio: Vec<ValidatedAudio>,
    pub(crate) warnings: Vec<String>,
}
impl ValidatedEdit {
    pub fn duration(&self) -> f64 {
        self.fps.seconds(self.duration_frames)
    }
    pub fn summary(&self) -> Value {
        json!({"version":1,"valid":true,"name":self.name,"width":self.width,"height":self.height,"fps":self.fps,
          "duration_seconds":self.duration(),"duration_frames":self.duration_frames,"clip_count":self.clips.len(),
          "audio_clip_count":self.audio.len(),"warnings":self.warnings})
    }
    pub fn source_paths(&self) -> impl Iterator<Item = &Path> {
        self.clips
            .iter()
            .filter_map(|c| c.source.as_deref())
            .chain(self.audio.iter().map(|a| a.source.as_path()))
    }
}

pub fn load_and_validate(path: &Path) -> Result<ValidatedEdit> {
    let edit: Edit = io::read_json(path)?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    validate(&edit, base)
}

pub fn validate(edit: &Edit, base_dir: &Path) -> Result<ValidatedEdit> {
    ensure!(
        edit.version == 1,
        "unsupported edit version {}; expected 1",
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
        !edit.clips.is_empty() && edit.clips.len() <= 512,
        "edit must contain between 1 and 512 primary clips"
    );
    ensure!(
        edit.audio.len() <= 256,
        "edit may contain at most 256 additional audio clips"
    );
    let mut sources = HashMap::<PathBuf, media::MediaInfo>::new();
    let mut clips = Vec::with_capacity(edit.clips.len());
    let mut total = 0u64;
    let mut warnings = Vec::new();
    for (index, clip) in edit.clips.iter().enumerate() {
        validate_numbers(clip.source_in, clip.duration, clip.speed, clip.volume)
            .with_context(|| format!("clip {index}"))?;
        let frames = quantize(
            edit.fps,
            clip.duration,
            &format!("clip {index} duration"),
            &mut warnings,
        )?;
        let duration = edit.fps.seconds(frames);
        let (source, kind, info) = if let Some(source) = &clip.source {
            let path = resolve(source, base_dir)?;
            let info = probe_cached(&path, &mut sources)?;
            ensure!(
                info.has_video(),
                "clip {index}: {} has no video/image stream; use the audio array for sound-only media",
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
                    "clip {index}: still images require source_in 0 and speed 1"
                );
            } else {
                check_range(
                    clip.source_in,
                    duration,
                    clip.speed,
                    &info,
                    &format!("clip {index}"),
                )?;
            }
            (Some(path), kind, Some(info))
        } else {
            ensure!(
                clip.source_in == 0.0 && clip.speed == 1.0,
                "gap clip {index}: source_in must be 0 and speed must be 1"
            );
            (None, MediaKind::Gap, None)
        };
        clips.push(ValidatedClip {
            source,
            kind,
            source_in: clip.source_in,
            offset_frames: total,
            duration_frames: frames,
            speed: clip.speed,
            volume: clip.volume,
            media: info,
        });
        total = total
            .checked_add(frames)
            .context("timeline frame count overflow")?;
        ensure!(
            edit.fps.seconds(total) <= 86_400.0,
            "timeline is limited to 24 hours"
        );
    }
    let mut audio = Vec::with_capacity(edit.audio.len());
    for (index, clip) in edit.audio.iter().enumerate() {
        validate_numbers(clip.source_in, clip.duration, clip.speed, clip.volume)
            .with_context(|| format!("audio clip {index}"))?;
        ensure!(
            clip.offset.is_finite() && clip.offset >= 0.0,
            "audio clip {index}: offset must be finite and nonnegative"
        );
        let frames = quantize(
            edit.fps,
            clip.duration,
            &format!("audio clip {index} duration"),
            &mut warnings,
        )?;
        let offset = edit.fps.frames(clip.offset);
        ensure!(
            offset.checked_add(frames).is_some_and(|end| end <= total),
            "audio clip {index} extends beyond the primary timeline"
        );
        if (edit.fps.seconds(offset) - clip.offset).abs() > 1e-7 {
            warnings.push(format!(
                "audio clip {index} offset rounded to output frame boundary"
            ));
        }
        let source = resolve(&clip.source, base_dir)?;
        let info = probe_cached(&source, &mut sources)?;
        ensure!(
            info.has_audio(),
            "audio clip {index}: {} has no audio stream",
            source.display()
        );
        check_range(
            clip.source_in,
            edit.fps.seconds(frames),
            clip.speed,
            &info,
            &format!("audio clip {index}"),
        )?;
        audio.push(ValidatedAudio {
            source,
            source_in: clip.source_in,
            offset_frames: offset,
            duration_frames: frames,
            speed: clip.speed,
            volume: clip.volume,
            media: info,
        });
    }
    Ok(ValidatedEdit {
        name: edit.name.clone(),
        width: edit.width,
        height: edit.height,
        fps: edit.fps,
        duration_frames: total,
        clips,
        audio,
        warnings,
    })
}

fn xml_text(value: &str) -> bool {
    value.chars().all(|c| {
        c == '\t' || c == '\n' || c == '\r' || (c >= ' ' && c != '\u{fffe}' && c != '\u{ffff}')
    })
}
fn validate_numbers(source_in: f64, duration: f64, speed: f64, volume: f64) -> Result<()> {
    ensure!(
        source_in.is_finite() && source_in >= 0.0,
        "source_in must be finite and nonnegative"
    );
    ensure!(
        duration.is_finite() && duration > 0.0 && duration <= 86_400.0,
        "duration must be finite and in (0, 86400] output seconds"
    );
    ensure!(
        speed.is_finite() && (0.125..=8.0).contains(&speed),
        "speed must be finite and between 0.125 and 8 source seconds per output second"
    );
    ensure!(
        volume.is_finite() && (0.0..=4.0).contains(&volume),
        "volume must be finite and between 0 and 4 (linear gain)"
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
    let info = media::probe(path)?;
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
    fn gap_edit() -> Edit {
        Edit {
            version: 1,
            name: "Example".into(),
            width: 320,
            height: 180,
            fps: Rational { num: 30, den: 1 },
            clips: vec![Clip {
                source: None,
                source_in: 0.0,
                duration: 1.0,
                speed: 1.0,
                volume: 1.0,
            }],
            audio: vec![],
        }
    }
    #[test]
    fn rational_ntsc_is_exact() {
        let fps = Rational {
            num: 30000,
            den: 1001,
        };
        assert_eq!(fps.fcpxml(1), "1001/30000s");
        assert_eq!(fps.fcpxml(30000), "1001s");
        assert_eq!(fps.frames(fps.seconds(12345)), 12345);
    }
    #[test]
    fn rejects_empty_invalid_and_unknown_edits() {
        let mut edit = gap_edit();
        edit.clips.clear();
        assert!(validate(&edit, Path::new(".")).is_err());
        edit = gap_edit();
        edit.clips[0].speed = f64::NAN;
        assert!(validate(&edit, Path::new(".")).is_err());
        edit = gap_edit();
        edit.fps.den = 0;
        assert!(validate(&edit, Path::new(".")).is_err());
        assert!(
            serde_json::from_value::<Edit>(json!({"clips":[],"transition":"dissolve"})).is_err()
        );
    }
    #[test]
    fn quantizes_and_validates_gaps() {
        let mut edit = gap_edit();
        edit.clips[0].duration = 1.01;
        let result = validate(&edit, Path::new(".")).unwrap();
        assert_eq!(result.duration_frames, 30);
        assert_eq!(result.warnings.len(), 1);
        edit.clips[0].source_in = 1.0;
        assert!(validate(&edit, Path::new(".")).is_err());
    }
}
