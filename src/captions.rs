//! Offline caption conversion and output-timeline metadata. No ASR or network calls.
use crate::{io, media, transcript::Transcript};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

pub const MAX_CUES: usize = 100_000;
const MAX_TEXT: usize = 4 * 1024 * 1024;
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptionMode {
    #[default]
    Soft,
    Burn,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptionTrack {
    pub source: PathBuf,
    #[serde(default = "language")]
    pub language: String,
    #[serde(default)]
    pub mode: CaptionMode,
    #[serde(default)]
    pub offset: f64,
    #[serde(default = "one")]
    pub scale: f64,
}
fn language() -> String {
    "und".into()
}
fn one() -> f64 {
    1.0
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chapter {
    pub start: f64,
    pub title: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct ValidatedTrack {
    pub source: PathBuf,
    pub inputs: Vec<PathBuf>,
    pub language: String,
    pub mode: CaptionMode,
    pub cues: Vec<Cue>,
}
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    pub offset: f64,
    pub scale: f64,
    pub max_words: usize,
    pub max_duration: f64,
    pub max_chars: usize,
}
impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            offset: 0.0,
            scale: 1.0,
            max_words: 8,
            max_duration: 4.0,
            max_chars: 80,
        }
    }
}
fn text_ok(s: &str) -> bool {
    !s.trim().is_empty()
        && s.chars()
            .all(|c| c == '\n' || c == '\t' || (c >= ' ' && c != '\u{fffe}' && c != '\u{ffff}'))
}
fn validate_mapping(offset: f64, scale: f64) -> Result<()> {
    ensure!(
        offset.is_finite()
            && offset.abs() <= 86400.0
            && scale.is_finite()
            && (0.125..=8.0).contains(&scale),
        "caption offset must be finite within 24 hours and scale between 0.125 and 8"
    );
    Ok(())
}
pub fn words(transcript: &Transcript, options: &ConvertOptions) -> Result<Vec<Cue>> {
    transcript.validate()?;
    validate_mapping(options.offset, options.scale)?;
    ensure!(
        (1..=100).contains(&options.max_words)
            && (1..=4096).contains(&options.max_chars)
            && options.max_duration.is_finite()
            && (0.1..=60.0).contains(&options.max_duration),
        "caption grouping requires 1–100 words, 1–4096 characters, and 0.1–60 seconds"
    );
    ensure!(
        !transcript.words.is_empty(),
        "caption conversion requires real word timestamps; transcript has no words"
    );
    let mut cues: Vec<Cue> = Vec::new();
    let mut count = 0;
    for word in &transcript.words {
        let start = options.offset + options.scale * word.start;
        let end = options.offset + options.scale * word.end;
        ensure!(
            start >= 0.0 && end > start && end <= 86400.0,
            "mapped word timestamp outside positive 24-hour timeline"
        );
        let word_text = word.text.split_whitespace().collect::<Vec<_>>().join(" ");
        ensure!(
            text_ok(&word_text) && word_text.chars().count() <= options.max_chars,
            "word text exceeds caption grouping character limit or contains controls"
        );
        let join = cues.last().is_some_and(|cue| {
            count < options.max_words
                && end.max(cue.end) - cue.start <= options.max_duration
                && cue.text.chars().count() + 1 + word_text.chars().count() <= options.max_chars
                && start <= cue.end + 1.0
        });
        if join {
            let cue = cues.last_mut().unwrap();
            cue.end = cue.end.max(end);
            write!(cue.text, " {word_text}")?;
            count += 1;
        } else {
            ensure!(
                end - start <= options.max_duration,
                "word duration exceeds caption grouping duration limit"
            );
            cues.push(Cue {
                start,
                end,
                text: word_text,
            });
            count = 1;
        }
    }
    validate_cues(&cues, 86400.0)?;
    Ok(cues)
}
pub fn convert(
    input: &Path,
    output: &Path,
    format: &str,
    options: &ConvertOptions,
    overwrite: bool,
) -> Result<Value> {
    let transcript = Transcript::load(input)?;
    let source = if transcript.source.is_absolute() {
        transcript.source.clone()
    } else {
        input
            .parent()
            .unwrap_or(Path::new("."))
            .join(&transcript.source)
    };
    io::protect_inputs(output, &[input.to_owned(), source])?;
    let cues = words(&transcript, options)?;
    let text = encode(&cues, format)?;
    io::atomic_write(output, text.as_bytes(), overwrite)?;
    Ok(
        json!({"output":output,"format":format,"cue_count":cues.len(),"offset":options.offset,"scale":options.scale,"transcription_requested":false}),
    )
}
// FFmpeg's WebVTT decoder escapes these for libass, but mov_text retains the
// escape bytes. Reject at the common input/serialization boundary rather than
// silently changing plain text in one of our renderers.
fn validate_literal_text(text: &str) -> Result<()> {
    ensure!(
        !text.contains(['{', '}', '\\']),
        "caption plain text cannot contain braces or backslashes: FFmpeg cannot preserve these identically in burn-in and soft subtitles"
    );
    Ok(())
}

pub fn encode(cues: &[Cue], format: &str) -> Result<String> {
    ensure!(
        matches!(format, "srt" | "vtt"),
        "caption format must be srt or vtt"
    );
    let mut out = if format == "vtt" {
        "WEBVTT\n\n".to_owned()
    } else {
        String::new()
    };
    for (i, cue) in cues.iter().enumerate() {
        validate_literal_text(&cue.text)?;
        // Treat caption text as plain text even in subtitle formats supporting markup.
        let text = cue
            .text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        writeln!(
            out,
            "{}\n{} --> {}\n{}\n",
            i + 1,
            stamp(cue.start, format == "srt"),
            stamp(cue.end, format == "srt"),
            text
        )?;
    }
    Ok(out)
}
fn stamp(t: f64, srt: bool) -> String {
    let ms = (t * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02}{}{:03}",
        ms / 3600000,
        ms / 60000 % 60,
        ms / 1000 % 60,
        if srt { ',' } else { '.' },
        ms % 1000
    )
}
fn parse_stamp(s: &str) -> Result<f64> {
    let s = s.trim().replace(',', ".");
    let p: Vec<_> = s.split(':').collect();
    ensure!(p.len() == 3, "caption timestamp must be HH:MM:SS.mmm");
    let h = p[0].parse::<u32>()?;
    let m = p[1].parse::<u32>()?;
    let sec = p[2].parse::<f64>()?;
    ensure!(
        m < 60 && (0.0..60.0).contains(&sec),
        "invalid caption timestamp"
    );
    Ok(h as f64 * 3600.0 + m as f64 * 60.0 + sec)
}
fn decode(text: &str) -> Result<Vec<Cue>> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let mut cues = Vec::new();
    for block in text.split("\n\n").filter(|b| !b.trim().is_empty()) {
        let lines: Vec<_> = block.lines().collect();
        if lines[0].trim() == "WEBVTT" {
            continue;
        }
        let ti = if lines[0].contains(" --> ") { 0 } else { 1 };
        ensure!(lines.len() > ti + 1, "caption block has no timing/text");
        let (start, end) = lines[ti]
            .split_once(" --> ")
            .context("caption block missing timing arrow")?;
        ensure!(
            !end.trim().contains(' '),
            "caption positioning/settings are unsupported; provide plain cues"
        );
        let text = lines[ti + 1..]
            .join("\n")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&");
        cues.push(Cue {
            start: parse_stamp(start)?,
            end: parse_stamp(end)?,
            text,
        });
        ensure!(cues.len() <= MAX_CUES, "too many caption cues");
    }
    Ok(cues)
}
pub(crate) fn validate_cues(cues: &[Cue], duration: f64) -> Result<()> {
    ensure!(
        !cues.is_empty() && cues.len() <= MAX_CUES,
        "caption track needs 1–100000 cues"
    );
    let mut previous = -1.0;
    let mut bytes = 0usize;
    for cue in cues {
        validate_literal_text(&cue.text)?;
        bytes = bytes
            .checked_add(cue.text.len())
            .context("caption size overflow")?;
        ensure!(
            bytes <= MAX_TEXT
                && cue.text.len() <= 4096
                && text_ok(&cue.text)
                && cue.text.lines().all(|line| !line.trim().is_empty()),
            "caption text exceeds limits or contains controls"
        );
        ensure!(
            cue.start.is_finite()
                && cue.end.is_finite()
                && cue.start >= 0.0
                && cue.start >= previous
                && cue.end > cue.start
                && cue.end <= duration + 0.001
                && (cue.end * 1000.0).round() > (cue.start * 1000.0).round(),
            "caption cues must be ordered, positive, millisecond-resolvable, and fit output timeline"
        );
        previous = cue.start;
    }
    Ok(())
}
pub(crate) fn load_tracks(
    tracks: &[CaptionTrack],
    base: &Path,
    duration: f64,
) -> Result<Vec<ValidatedTrack>> {
    ensure!(tracks.len() <= 8, "at most 8 caption tracks are supported");
    let mut loaded = Vec::new();
    let mut all_cues = 0;
    for track in tracks {
        validate_mapping(track.offset, track.scale)?;
        ensure!(
            track.language.len() == 3 && track.language.bytes().all(|c| c.is_ascii_lowercase()),
            "caption language must be a three-letter lowercase ISO-639 code (or und)"
        );
        let source = media::canonical_file(&base.join(&track.source))?;
        let bytes =
            io::read_files_bounded(std::slice::from_ref(&source), 8 * 1024 * 1024)?.remove(0);
        let mut inputs = Vec::new();
        let mut cues = match source.extension().and_then(|e| e.to_str()) {
            Some("srt" | "vtt") => {
                decode(std::str::from_utf8(&bytes).context("captions must be UTF-8")?)?
            }
            Some("json") => {
                let transcript: Transcript = serde_json::from_slice(&bytes)?;
                let dependency = if transcript.source.is_absolute() {
                    transcript.source.clone()
                } else {
                    source.parent().unwrap_or(base).join(&transcript.source)
                };
                inputs.push(dependency);
                words(&transcript, &ConvertOptions::default())?
            }
            _ => bail!("caption source must be .srt, .vtt, or a word-timestamp transcript .json"),
        };
        for cue in &mut cues {
            cue.start = track.offset + track.scale * cue.start;
            cue.end = track.offset + track.scale * cue.end;
        }
        validate_cues(&cues, duration)?;
        all_cues += cues.len();
        ensure!(
            all_cues <= MAX_CUES,
            "combined caption tracks exceed 100000 cues"
        );
        loaded.push(ValidatedTrack {
            source,
            inputs,
            language: track.language.clone(),
            mode: track.mode,
            cues,
        });
    }
    Ok(loaded)
}
pub(crate) fn validate_chapters(chapters: &[Chapter], duration: f64) -> Result<()> {
    ensure!(
        chapters.len() <= 1000,
        "at most 1000 chapters are supported"
    );
    let mut prev = -1.0;
    for c in chapters {
        ensure!(
            c.start.is_finite()
                && c.start >= 0.0
                && c.start < duration
                && c.start > prev
                && (c.start * 1000.0).round() < (duration * 1000.0).round()
                && text_ok(&c.title)
                && c.title.len() <= 512,
            "chapter starts must be finite, strictly ordered and inside output timeline; titles need 1–512 bytes without controls"
        );
        ensure!(
            prev < 0.0 || (c.start * 1000.0).round() > (prev * 1000.0).round(),
            "chapter starts must be distinct at millisecond precision"
        );
        prev = c.start;
    }
    Ok(())
}
pub(crate) fn chapter_metadata(chapters: &[Chapter], duration: f64) -> String {
    // FFmpeg's line continuation parser cannot round-trip a title ending in a
    // backslash. Keep this sidecar exclusively structural; renderers supply
    // exact titles with separate metadata arguments and project JSON keeps them.
    let mut out =
        ";FFMETADATA1\n; Chapter boundaries; exact titles are in captions-chapters.json\n"
            .to_owned();
    for (i, c) in chapters.iter().enumerate() {
        let end = chapters.get(i + 1).map_or(duration, |c| c.start);
        write!(
            out,
            "[CHAPTER]\nTIMEBASE=1/1000\nSTART={}\nEND={}\n",
            (c.start * 1000.0).round() as u64,
            (end * 1000.0).round() as u64,
        )
        .unwrap();
    }
    out
}

pub fn mux(
    input: &Path,
    edit: &crate::timeline::ValidatedEdit,
    output: &Path,
    overwrite: bool,
) -> Result<Value> {
    crate::export::mux_captions(input, edit, output, overwrite)
}
