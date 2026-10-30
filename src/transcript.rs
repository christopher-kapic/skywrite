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
            if word.text.trim().is_empty()
                || word.text.len() > 4096
                || word.text.chars().any(|c| c == '\0')
            {
                bail!("invalid transcript word text");
            }
            if !word.start.is_finite()
                || !word.end.is_finite()
                || word.start < 0.0
                || word.end < word.start
                || word.end > self.duration + 0.15
                || word.start < previous
            {
                bail!("invalid or unsorted word timestamps");
            }
            if word
                .confidence
                .is_some_and(|c| !c.is_finite() || !(0.0..=1.0).contains(&c))
            {
                bail!("word confidence must be between zero and one");
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
        let (text, mut chunk_words) = parse_response(&response.body, length)
            .with_context(|| format!("transcription chunk {}", index + 1))?;
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
    let transcript = Transcript {
        version: 1,
        source: info.path.clone(),
        duration: info.duration,
        text: texts.join(" "),
        words,
        model: Some(options.model.to_owned()),
        usage: Some(
            json!({"chunks":usages,"cache_hits":cache_hits,"total_chunks":chunks,"source_sha256":source_sha256}),
        ),
    };
    transcript.validate()?;
    Ok(transcript)
}
fn parse_response(response: &Value, duration: f64) -> Result<(String, Vec<Word>)> {
    let text = response["text"]
        .as_str()
        .context("transcription response has no text")?
        .trim()
        .to_owned();
    let raw_words = response.get("words").and_then(Value::as_array);
    if raw_words.is_none() && !text.is_empty() {
        bail!(
            "provider returned text without word timestamps; choose a timestamp-capable transcription model (no timestamps were fabricated)"
        );
    }
    let mut words = Vec::new();
    if let Some(raw_words) = raw_words {
        for value in raw_words {
            let word: Word =
                serde_json::from_value(value.clone()).context("invalid provider word timestamp")?;
            words.push(word);
        }
    }
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
    Ok((text, words))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_real_word_timestamps_and_rejects_text_only() {
        let (_, words) = parse_response(&json!({"text":"hello world", "words":[{"word":"hello","start":0.1,"end":0.4},{"word":"world","start":0.5,"end":0.9}]}), 1.0).unwrap();
        assert_eq!(words[0].text, "hello");
        assert_eq!(words[1].start, 0.5);
        assert!(parse_response(&json!({"text":"hello world"}), 1.0).is_err());
        assert!(parse_response(&json!({"text":"hello","words":[]}), 1.0).is_err());
        assert!(parse_response(&json!({"text":"","words":[]}), 1.0).is_ok());
    }
    #[test]
    fn rejects_unsorted_and_out_of_range_words() {
        assert!(
            parse_response(
                &json!({"text":"bad","words":[{"word":"bad","start":2.0,"end":3.0}]}),
                1.0
            )
            .is_err()
        );
        assert!(parse_response(&json!({"text":"bad","words":[{"word":"a","start":0.6,"end":0.7},{"word":"b","start":0.2,"end":0.3}]}),1.0).is_err());
    }
}
