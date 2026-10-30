use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Word {
    #[serde(alias = "word")]
    pub text: String,
    pub start: f64,
    pub end: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    #[serde(default = "version_one")]
    pub version: u32,
    pub source: PathBuf,
    pub duration: f64,
    pub text: String,
    pub words: Vec<Word>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
}
fn version_one() -> u32 {
    1
}
impl Transcript {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported transcript version {}", self.version);
        }
        if !self.duration.is_finite() || self.duration <= 0.0 {
            bail!("transcript duration must be finite and positive");
        }
        if self.words.len() > 1_000_000 {
            bail!("transcript exceeds one million words");
        }
        let mut previous = -1.0;
        for word in &self.words {
            validate_word(word, self.duration)?;
            if word.start < previous {
                bail!("invalid or unsorted word timestamps");
            }
            previous = word.start;
        }
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        let transcript: Self = crate::io::read_json(path)?;
        transcript.validate()?;
        Ok(transcript)
    }
}

// Shared by strict native transcripts and external provider ingestion. Ordering is
// a separate invariant: overlapping speakers are valid, malformed words are not.
fn validate_word(word: &Word, duration: f64) -> Result<()> {
    if word.text.trim().is_empty() || word.text.len() > 4096 || word.text.chars().any(|c| c == '\0')
    {
        bail!("invalid transcript word text");
    }
    if !word.start.is_finite()
        || !word.end.is_finite()
        || word.start < 0.0
        || word.end < word.start
        || word.end > duration + 0.15
    {
        bail!("invalid word timestamps");
    }
    if word
        .confidence
        .is_some_and(|c| !c.is_finite() || !(0.0..=1.0).contains(&c))
    {
        bail!("word confidence must be between zero and one");
    }
    Ok(())
}

// Stable sorting retains provider order for concurrent words with equal starts.
// Call only after individual words have passed validation; no times are changed.
fn order_words(words: &mut [Word]) -> bool {
    let reordered = words.windows(2).any(|pair| pair[0].start > pair[1].start);
    if reordered {
        words.sort_by(|a, b| {
            a.start
                .partial_cmp(&b.start)
                .expect("word timestamps have been validated as finite")
        });
    }
    reordered
}

pub struct TranscribeOptions<'a> {
    pub model: &'a str,
    pub language: Option<&'a str>,
    pub start: f64,
    pub duration: Option<f64>,
    pub chunk_seconds: f64,
}
pub fn transcribe(
    info: &crate::media::MediaInfo,
    api: &crate::openrouter::OpenRouter,
    options: &TranscribeOptions<'_>,
) -> Result<Transcript> {
    if !info.has_audio() {
        bail!("media has no audio stream");
    }
    let duration = options.duration.unwrap_or(info.duration - options.start);
    crate::media::validate_range(info, options.start, duration)?;
    if !(10.0..=120.0).contains(&options.chunk_seconds) {
        bail!("chunk seconds must be between 10 and 120");
    }
    if duration > 24.0 * 3600.0 {
        bail!("transcription range exceeds 24 hours; split the recording");
    }
    let source_sha256 = crate::media::file_sha256(&info.path)?;
    let source_modified = std::fs::metadata(&info.path)?.modified().ok();
    let temp = tempfile::tempdir()?;
    let mut words = Vec::new();
    let mut texts = Vec::new();
    let mut usages = Vec::new();
    let mut cache_hits = 0;
    let mut provider_reordered_chunks = 0;
    let chunks = (duration / options.chunk_seconds).ceil() as usize;
    for index in 0..chunks {
        let offset = options.start + index as f64 * options.chunk_seconds;
        let length = (options.start + duration - offset).min(options.chunk_seconds);
        let audio_path = temp.path().join(format!("chunk-{index:05}.wav"));
        crate::media::extract_audio(info, &audio_path, offset, length)?;
        let bytes = std::fs::read(&audio_path)?;
        if bytes.len() > 4 * 1024 * 1024 {
            bail!("encoded audio chunk exceeded 4 MiB");
        }
        let mut request = json!({"model": options.model, "input_audio":{"data":STANDARD.encode(bytes),"format":"wav"},"response_format":"verbose_json","timestamp_granularities":["word"]});
        if let Some(language) = options.language {
            request["language"] = Value::String(language.to_owned());
        }
        eprintln!(
            "Transcribing chunk {}/{} ({offset:.3}s + {length:.3}s)",
            index + 1,
            chunks
        );
        let response = api.request("audio/transcriptions", &request)?;
        if response.cached {
            cache_hits += 1;
        }
        let (text, mut chunk_words, reordered) = parse_response(&response.body, length)
            .with_context(|| format!("transcription chunk {}", index + 1))?;
        if reordered {
            provider_reordered_chunks += 1;
            eprintln!(
                "Transcription chunk {}: reordered provider words chronologically; original timestamps and text preserved",
                index + 1
            );
        }
        for word in &mut chunk_words {
            word.start += offset;
            word.end += offset;
        }
        texts.push(text);
        words.extend(chunk_words);
        if let Some(usage) = response.body.get("usage") {
            usages.push(json!({"cached":response.cached,"usage":usage}));
        }
    }
    if std::fs::metadata(&info.path)?.modified().ok() != source_modified {
        bail!("source changed during transcription; discard mixed-source results and retry");
    }
    // The provider permits a small end-of-chunk tolerance, so a word may
    // start after the next chunk begins. Restore chronology after offsetting.
    let chunk_boundary_reordered = order_words(&mut words);
    if chunk_boundary_reordered {
        eprintln!(
            "Transcription: reordered words across chunk boundaries chronologically; original timestamps and text preserved"
        );
    }
    let transcript = Transcript {
        version: 1,
        source: info.path.clone(),
        duration: info.duration,
        text: texts.join(" "),
        words,
        model: Some(options.model.to_owned()),
        usage: Some(
            json!({"chunks":usages,"cache_hits":cache_hits,"total_chunks":chunks,"source_sha256":source_sha256,"word_ordering":{"method":"stable_start_time","provider_reordered_chunks":provider_reordered_chunks,"chunk_boundary_reordered":chunk_boundary_reordered}}),
        ),
    };
    transcript.validate()?;
    Ok(transcript)
}
fn parse_response(response: &Value, duration: f64) -> Result<(String, Vec<Word>, bool)> {
    let text = response["text"]
        .as_str()
        .context("transcription response has no text")?
        .trim()
        .to_owned();
    let raw_words = response
        .get("words")
        .map(|value| value.as_array().context("invalid provider words array"))
        .transpose()?;
    if raw_words.is_none() && !text.is_empty() {
        bail!(
            "provider returned text without word timestamps; choose a timestamp-capable transcription model (no timestamps were fabricated)"
        );
    }
    let mut words = Vec::new();
    if let Some(raw_words) = raw_words {
        if raw_words.len() > 1_000_000 {
            bail!("transcript exceeds one million words");
        }
        for value in raw_words {
            let word: Word =
                serde_json::from_value(value.clone()).context("invalid provider word timestamp")?;
            validate_word(&word, duration)?;
            words.push(word);
        }
    }
    let reordered = order_words(&mut words);
    let check = Transcript {
        version: 1,
        source: PathBuf::new(),
        duration,
        text: text.clone(),
        words: words.clone(),
        model: None,
        usage: None,
    };
    check.validate()?;
    if !text.is_empty() && words.is_empty() {
        bail!("provider returned nonempty text but no word timestamps");
    }
    Ok((text, words, reordered))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_real_word_timestamps_and_rejects_text_only() {
        let (_, words, reordered) = parse_response(&json!({"text":"hello world", "words":[{"word":"hello","start":0.1,"end":0.4},{"word":"world","start":0.5,"end":0.9}]}), 1.0).unwrap();
        assert!(!reordered);
        assert_eq!(words[0].text, "hello");
        assert_eq!(words[1].start, 0.5);
        assert!(parse_response(&json!({"text":"hello world"}), 1.0).is_err());
        assert!(parse_response(&json!({"text":"hello","words":[]}), 1.0).is_err());
        assert!(parse_response(&json!({"text":"","words":[]}), 1.0).is_ok());
        assert!(parse_response(&json!({"text":"","words":null}), 1.0).is_err());
        assert!(parse_response(&json!({"text":"","words":{"word":"hidden"}}), 1.0).is_err());
    }
    #[test]
    fn provider_overlap_and_equal_starts_preserve_all_words_and_text() {
        let (text, words, reordered) = parse_response(
            &json!({
                "text":"alright. Are Are you", "words":[
                    {"word":"alright.","start":10.52,"end":11.48},
                    {"word":"Are","start":10.5,"end":13.34},
                    {"word":"Are","start":10.5,"end":11.0},
                    {"word":"you","start":13.34,"end":13.34}
                ]
            }),
            14.0,
        )
        .unwrap();
        assert!(reordered);
        assert_eq!(text, "alright. Are Are you");
        assert_eq!(words.len(), 4);
        assert_eq!(
            (words[0].text.as_str(), words[0].start, words[0].end),
            ("Are", 10.5, 13.34)
        );
        assert_eq!(
            (words[1].text.as_str(), words[1].start, words[1].end),
            ("Are", 10.5, 11.0)
        );
        assert_eq!(
            (words[2].text.as_str(), words[2].start, words[2].end),
            ("alright.", 10.52, 11.48)
        );
        assert_eq!((words[3].start, words[3].end), (13.34, 13.34));
    }

    #[test]
    fn provider_rejects_invalid_words_instead_of_dropping_or_clamping() {
        for word in [
            json!({"word":"negative","start":-0.1,"end":0.2}),
            json!({"word":"reversed","start":0.6,"end":0.2}),
            json!({"word":"outside","start":0.5,"end":1.151}),
            json!({"word":"nonfinite","start":"NaN","end":0.2}),
            json!({"word":"null","start":null,"end":0.2}),
            json!({"word":"","start":0.1,"end":0.2}),
            json!({"word":"confidence","start":0.1,"end":0.2,"confidence":1.1}),
        ] {
            assert!(
                parse_response(
                    &json!({"text":"bad","words":[
                        {"word":"valid","start":0.8,"end":0.9}, word
                    ]}),
                    1.0
                )
                .is_err()
            );
        }
        let mut word = Word {
            text: "finite".into(),
            start: 0.1,
            end: 0.2,
            confidence: None,
        };
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            word.start = invalid;
            assert!(validate_word(&word, 1.0).is_err());
            word.start = 0.1;
            word.end = invalid;
            assert!(validate_word(&word, 1.0).is_err());
            word.end = 0.2;
        }
    }

    #[test]
    fn native_transcript_load_still_rejects_unsorted_words() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unsorted.json");
        std::fs::write(&path, serde_json::to_vec(&json!({"source":"speech.wav","duration":1.0,
            "text":"a b","words":[{"word":"a","start":0.6,"end":0.7},{"word":"b","start":0.2,"end":0.3}]
        })).unwrap()).unwrap();
        assert!(Transcript::load(&path).is_err());
    }
}
