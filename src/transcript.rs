use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_WORDS: usize = 1_000_000;

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
        if self.words.len() > MAX_WORDS {
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

// Count without retaining serialized copies. The newline count permits exact
// pretty-JSON accounting when a value is nested more deeply in the transcript.
#[derive(Default)]
struct JsonSize {
    bytes: usize,
    newlines: usize,
}
impl Write for JsonSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("JSON size overflow"))?;
        self.newlines += bytes.iter().filter(|&&b| b == b'\n').count();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn json_size(value: &impl Serialize, depth: usize) -> Result<usize> {
    let mut size = JsonSize::default();
    serde_json::to_writer_pretty(&mut size, value)?;
    size.bytes
        .checked_add(
            size.newlines
                .checked_mul(depth * 2)
                .context("JSON size overflow")?,
        )
        .context("JSON size overflow")
}
#[derive(Clone, Copy, Serialize)]
struct ChunkUsage<'a> {
    cached: bool,
    usage: &'a Value,
}

// This is a single operation-wide admission boundary. Each response is bounded
// separately by OpenRouter; nothing from an excess chunk enters retained state.
struct TranscriptBudget {
    bytes: usize,
    words: usize,
    texts: usize,
    usages: usize,
    max_bytes: usize,
    max_words: usize,
}
impl TranscriptBudget {
    fn new(base_bytes: usize, max_bytes: usize, max_words: usize) -> Result<Self> {
        if base_bytes > max_bytes {
            bail!("transcript exceeds serialized output budget");
        }
        Ok(Self {
            bytes: base_bytes,
            words: 0,
            texts: 0,
            usages: 0,
            max_bytes,
            max_words,
        })
    }
    fn check_words(&self, count: usize) -> Result<()> {
        if count > self.max_words.saturating_sub(self.words) {
            bail!("transcript exceeds cumulative word budget (maximum one million words)");
        }
        Ok(())
    }
    fn admit(&mut self, text: &str, words: &[Word], usage: Option<ChunkUsage<'_>>) -> Result<()> {
        self.check_words(words.len())?;
        // Strings cannot contain literal JSON newlines. Exclude the two quotes
        // already present in the empty transcript's text field.
        let mut added = json_size(&text, 0)? - 2 + usize::from(self.texts > 0);
        for (index, word) in words.iter().enumerate() {
            let overhead = if self.words == 0 && index == 0 { 8 } else { 6 };
            added = added
                .checked_add(json_size(word, 2)? + overhead)
                .context("JSON size overflow")?;
        }
        if let Some(usage) = usage {
            let overhead = if self.usages == 0 { 12 } else { 8 };
            added = added
                .checked_add(json_size(&usage, 3)? + overhead)
                .context("JSON size overflow")?;
        }
        let next = self
            .bytes
            .checked_add(added)
            .context("JSON size overflow")?;
        if next > self.max_bytes {
            bail!("transcript exceeds cumulative serialized output budget (64 MiB)");
        }
        self.bytes = next;
        self.words += words.len();
        self.texts += 1;
        self.usages += usize::from(usage.is_some());
        Ok(())
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
    transcribe_with_limits(
        info,
        api,
        options,
        crate::io::MAX_JSON_BYTES as usize,
        MAX_WORDS,
    )
}
fn transcribe_with_limits(
    info: &crate::media::MediaInfo,
    api: &crate::openrouter::OpenRouter,
    options: &TranscribeOptions<'_>,
    max_bytes: usize,
    max_words: usize,
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
    // Reserve counters at their largest possible decimal width and the longer
    // false spelling. All other fields have their final values already.
    let baseline = Transcript {
        version: 1,
        source: info.path.clone(),
        duration: info.duration,
        text: String::new(),
        words: Vec::new(),
        model: Some(options.model.to_owned()),
        usage: Some(
            json!({"chunks":[],"cache_hits":chunks,"total_chunks":chunks,"source_sha256":source_sha256,"word_ordering":{"method":"stable_start_time","provider_reordered_chunks":chunks,"chunk_boundary_reordered":false}}),
        ),
    };
    let mut budget = TranscriptBudget::new(json_size(&baseline, 0)? + 1, max_bytes, max_words)?;
    for index in 0..chunks {
        let offset = options.start + index as f64 * options.chunk_seconds;
        let length = (options.start + duration - offset).min(options.chunk_seconds);
        let audio_path = temp.path().join(format!("chunk-{index:05}.wav"));
        crate::media::extract_audio(info, &audio_path, offset, length)?;
        let bytes = std::fs::read(&audio_path)?;
        // Each WAV is only needed until it has been encoded for this request.
        std::fs::remove_file(&audio_path).context("removing processed audio chunk")?;
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
        // Inspect the array count before deserializing or sorting extra words.
        if let Some(raw_words) = response.body.get("words").and_then(Value::as_array) {
            budget.check_words(raw_words.len())?;
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
        budget.admit(
            &text,
            &chunk_words,
            response.body.get("usage").map(|usage| ChunkUsage {
                cached: response.cached,
                usage,
            }),
        )?;
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
        if raw_words.len() > MAX_WORDS {
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
    #[test]
    fn incremental_budget_matches_pretty_output_and_accepts_exact_limits() {
        let mut value = json!({"text":"", "words":[], "usage":{"chunks":[]}});
        let base = json_size(&value, 0).unwrap() + 1;
        let word = Word {
            text: "quote\" newline\n雪".into(),
            start: 86400.14,
            end: 86400.15,
            confidence: Some(0.9),
        };
        let usage = json!({"deep":[{"nested":[1,2,{"x":"\n"}]}]});
        let mut budget = TranscriptBudget::new(base, 4096, 3).unwrap();
        let mut all_words = Vec::new();
        let mut all_usage = Vec::new();
        let mut texts = Vec::new();
        for (text, words, usage) in [
            ("", vec![], None),
            ("hello\n雪", vec![word.clone(), word.clone()], Some(&usage)),
            ("", vec![], Some(&json!({}))),
            ("end", vec![word.clone()], None),
        ] {
            budget
                .admit(
                    text,
                    &words,
                    usage.map(|usage| ChunkUsage {
                        cached: true,
                        usage,
                    }),
                )
                .unwrap();
            texts.push(text);
            all_words.extend(words);
            if let Some(usage) = usage {
                all_usage.push(json!({"cached":true,"usage":usage}));
            }
            value["text"] = json!(texts.join(" "));
            value["words"] = json!(all_words);
            value["usage"]["chunks"] = json!(all_usage);
            assert_eq!(
                budget.bytes,
                serde_json::to_vec_pretty(&value).unwrap().len() + 1
            );
        }
        let mut exact = TranscriptBudget::new(base, base, 0).unwrap();
        exact.admit("", &[], None).unwrap(); // Empty first chunks consume no bytes.
        let before = (exact.bytes, exact.words, exact.texts, exact.usages);
        assert!(
            exact
                .admit("", &[], None)
                .unwrap_err()
                .to_string()
                .contains("serialized output budget")
        );
        assert_eq!(
            (exact.bytes, exact.words, exact.texts, exact.usages),
            before
        );
        assert!(
            budget
                .admit("more", &[word], None)
                .unwrap_err()
                .to_string()
                .contains("word budget")
        );
        assert_eq!(budget.words, 3);
    }
    #[test]
    fn text_and_usage_budget_reject_first_excess_without_retention() {
        for usage in [false, true] {
            let data = json!({"data":"large enough"});
            let mut accepted = TranscriptBudget::new(10, 4096, 1).unwrap();
            let incoming_usage = usage.then_some(ChunkUsage {
                cached: false,
                usage: &data,
            });
            let text = if usage { "" } else { "large enough" };
            accepted.admit(text, &[], incoming_usage).unwrap();
            let mut exact = TranscriptBudget::new(10, accepted.bytes, 1).unwrap();
            exact.admit(text, &[], incoming_usage).unwrap();
            let mut over = TranscriptBudget::new(10, accepted.bytes - 1, 1).unwrap();
            assert!(
                over.admit(text, &[], incoming_usage)
                    .unwrap_err()
                    .to_string()
                    .contains("serialized output budget")
            );
            assert_eq!((over.bytes, over.texts, over.usages), (10, 0, 0));
        }
    }
    #[test]
    fn actual_multichunk_stops_before_third_http_request_on_word_budget() {
        use std::io::{Read, Write};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.wav");
        assert!(
            std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-nostdin",
                    "-f",
                    "lavfi",
                    "-i",
                    "anullsrc=r=16000:cl=mono",
                    "-t",
                    "30",
                    "-c:a",
                    "pcm_s16le"
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let info = crate::media::probe(&source).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let api = crate::openrouter::OpenRouter::for_test(
            format!("http://{}", listener.local_addr().unwrap()),
            None,
            std::time::Duration::from_secs(2),
        );
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(std::time::Duration::from_millis(5))
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut headers = Vec::new();
                let mut byte = [0];
                while !headers.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                    assert!(headers.len() < 8192);
                }
                let header = String::from_utf8(headers).unwrap();
                let len: usize = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|s| s.trim().parse().unwrap())
                    })
                    .unwrap();
                let mut body = vec![0; len];
                stream.read_exact(&mut body).unwrap();
                count.fetch_add(1, Ordering::SeqCst);
                let body=serde_json::to_vec(&json!({"text":"second first","words":[{"word":"second","start":0.5,"end":0.8},{"word":"first","start":0.1,"end":0.3}],"usage":{"cost":0}})).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        let options = TranscribeOptions {
            model: "fixture",
            language: None,
            start: 0.,
            duration: None,
            chunk_seconds: 10.,
        };
        let error = transcribe_with_limits(&info, &api, &options, 4096, 3).unwrap_err();
        server.join().unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert!(
            error.to_string().contains("cumulative word budget"),
            "{error:#}"
        );
    }
    #[test]
    fn cached_multichunk_exact_output_budget_keeps_order_usage_and_empty_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.wav");
        assert!(
            std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-nostdin",
                    "-f",
                    "lavfi",
                    "-i",
                    "aevalsrc=0.1*sin(2*PI*(317*t+2*t*t)):s=16000",
                    "-t",
                    "30",
                    "-c:a",
                    "pcm_s16le"
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let info = crate::media::probe(&source).unwrap();
        let cache = dir.path().join("cache");
        std::fs::create_dir(&cache).unwrap();
        for index in 0..3 {
            let chunk = dir.path().join(format!("chunk{index}.wav"));
            crate::media::extract_audio(&info, &chunk, index as f64 * 10., 10.).unwrap();
            let request = json!({"model":"fixture","input_audio":{"data":STANDARD.encode(std::fs::read(chunk).unwrap()),"format":"wav"},"response_format":"verbose_json","timestamp_granularities":["word"]});
            let key = crate::openrouter::cache_key(&[
                b"https://openrouter.ai/api/v1",
                b"audio/transcriptions",
                &serde_json::to_vec(&request).unwrap(),
            ]);
            let response = if index == 1 {
                json!({"text":"","words":[],"usage":{"nested":[1,{"x":"\n雪"}]}})
            } else {
                json!({"text":"second first","words":[{"word":"second","start":0.5,"end":0.8},{"word":"first","start":0.1,"end":0.3}],"usage":{"cost":0}})
            };
            std::fs::write(
                cache.join(format!("{key}.json")),
                serde_json::to_vec(&response).unwrap(),
            )
            .unwrap();
        }
        let api = crate::openrouter::OpenRouter::new(Some(&cache)).unwrap();
        let options = TranscribeOptions {
            model: "fixture",
            language: None,
            start: 0.,
            duration: None,
            chunk_seconds: 10.,
        };
        let accepted = transcribe_with_limits(&info, &api, &options, 4096, 4).unwrap();
        let exact = json_size(&accepted, 0).unwrap() + 1;
        let transcript = transcribe_with_limits(&info, &api, &options, exact, 4).unwrap();
        assert_eq!(transcript.text, "second first  second first");
        assert_eq!(
            transcript
                .words
                .iter()
                .map(|w| (w.text.as_str(), w.start, w.end))
                .collect::<Vec<_>>(),
            vec![
                ("first", 0.1, 0.3),
                ("second", 0.5, 0.8),
                ("first", 20.1, 20.3),
                ("second", 20.5, 20.8)
            ]
        );
        let usage = transcript.usage.unwrap();
        assert_eq!(usage["cache_hits"], 3);
        assert_eq!(usage["word_ordering"]["provider_reordered_chunks"], 2);
        assert_eq!(usage["chunks"].as_array().unwrap().len(), 3);
        assert!(
            transcribe_with_limits(&info, &api, &options, exact - 1, 4)
                .unwrap_err()
                .to_string()
                .contains("serialized output budget")
        );
    }
}
