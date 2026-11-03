use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub struct Analysis {
    pub version: u32,
    pub media: crate::media::MediaInfo,
    pub range: Value,
    pub frames: Vec<crate::media::SampleFrame>,
    pub coverage: Value,
    pub transcript: Option<crate::transcript::Transcript>,
    pub understanding: Option<Value>,
    pub model: Option<String>,
    pub usage: Option<Value>,
    pub cached: bool,
    #[serde(default)]
    pub source_sha256: String,
    #[serde(default)]
    pub frames_cached: bool,
}
pub struct AnalyzeOptions<'a> {
    pub frames: usize,
    pub frames_dir: &'a Path,
    pub start: f64,
    pub duration: Option<f64>,
    pub model: Option<&'a str>,
    pub transcript: Option<&'a Path>,
    pub prompt: Option<&'a str>,
}
pub fn analyze(
    info: crate::media::MediaInfo,
    api: Option<&crate::openrouter::OpenRouter>,
    options: &AnalyzeOptions<'_>,
) -> Result<Analysis> {
    let info = crate::media::resolve_audio_only_timing(info)?;
    let duration = options.duration.unwrap_or(if info.duration > 0.0 {
        info.duration - options.start
    } else {
        1.0
    });
    crate::media::validate_range(&info, options.start, duration)?;
    if options.prompt.is_some_and(|p| p.len() > 16 * 1024) {
        bail!("editorial prompt exceeds 16 KiB");
    }
    let source_sha256 = crate::media::file_sha256(&info.path)?;
    let mut transcript = options
        .transcript
        .map(crate::transcript::Transcript::load)
        .transpose()?;
    if let Some(transcript) = &transcript {
        transcript.verify_source(
            &info.path,
            options
                .transcript
                .and_then(Path::parent)
                .unwrap_or(Path::new(".")),
            &source_sha256,
        )?;
    }
    if let Some(transcript) = &mut transcript {
        // Reports describe the currently selected file; the original transcript
        // remains untouched and hash verification permits identical copies.
        transcript.source = info.path.clone();
    }
    let (frames, frames_cached) = crate::cache::sampled_frames(
        &info,
        options.frames_dir,
        &source_sha256,
        options.frames,
        options.start,
        duration,
    )?;
    let run_dir = tempfile::tempdir()?;
    let audio_duration =
        if api.is_some() && info.has_audio() && !info.has_video() && transcript.is_none() {
            duration.min(60.0)
        } else {
            0.0
        };
    let coverage = json!({
        "visual":"sparse uniformly sampled frames only; unsampled events and exact shot boundaries are unknown",
        "sample_times":frames.iter().map(|f| f.time).collect::<Vec<_>>(),
        "sample_time_semantics":"requested seek positions; decoded video frames can fall near those positions, precise decoded frame PTS is not measured",
        "audio_sample_start":if audio_duration>0.0 { Some(options.start) } else { None },
        "audio_sample_duration":audio_duration,
        "transcript_available":transcript.is_some(),
        "transcript_content_identity_verified":transcript.as_ref().and_then(|t|t.usage.as_ref()).and_then(|u|u.get("source_sha256")).is_some(),
        "timestamp_origin":"seconds relative to source media playback start, not container timecode",
    });
    let mut analysis = Analysis {
        version: 1,
        media: info,
        range: json!({"start":options.start,"duration":duration}),
        frames,
        coverage,
        transcript,
        understanding: None,
        model: options.model.map(str::to_owned),
        usage: None,
        cached: false,
        source_sha256,
        frames_cached,
    };
    if let Some(api) = api {
        let model = options.model.context("remote analysis requires --model or OPENROUTER_VISION_MODEL; use --local for frame sampling without API requests")?;
        let mut content =
            vec![json!({"type":"text","text":analysis_prompt(&analysis, options.prompt)?})];
        for frame in &analysis.frames {
            let bytes = std::fs::read(&frame.path)?;
            if bytes.len() > 1024 * 1024 {
                bail!("sample frame exceeds one MiB");
            }
            content.push(
                json!({"type":"text","text":format!("Source frame at {:.6} seconds",frame.time)}),
            );
            content.push(json!({"type":"image_url","image_url":{"url":format!("data:image/jpeg;base64,{}",STANDARD.encode(bytes))}}));
        }
        if audio_duration > 0.0 {
            let audio = run_dir.path().join("audio-sample.wav");
            crate::media::extract_audio(&analysis.media, &audio, options.start, audio_duration)?;
            let bytes = std::fs::read(audio)?;
            content.push(json!({"type":"input_audio","input_audio":{"data":STANDARD.encode(bytes),"format":"wav"}}));
        }
        let request = json!({"model":model,"messages":[{"role":"user","content":content}],"response_format":{"type":"json_object"},"max_tokens":2500,"temperature":0.2});
        let response = api.request("chat/completions", &request)?;
        let understanding = crate::openrouter::assistant_json(&response.body)?;
        validate_understanding(&understanding, options.start, duration)?;
        analysis.understanding = Some(understanding);
        analysis.usage = response.body.get("usage").cloned();
        analysis.cached = response.cached;
    }
    Ok(analysis)
}
const MAX_CONTEXT_WORDS: usize = 8192;
const MAX_WORD_CONTEXT_BYTES: usize = 512 * 1024;
const MAX_SUGGEST_CONTEXT_BYTES: usize = 8 * 1024 * 1024;

/// Compact dialogue evidence; no transcript summary substitutes for alignment.
/// Fail explicitly instead of silently omitting words when a range is too large.
fn word_context(analysis: &Analysis) -> Result<Value> {
    let Some(transcript) = &analysis.transcript else {
        return Ok(Value::Null);
    };
    let (start, end) = analyzed_range(analysis)?;
    let words = transcript.words_in_range(start, end, MAX_CONTEXT_WORDS)?;
    let mut bytes = 0usize;
    for word in &words {
        bytes += serde_json::to_vec(word)?.len() + 1;
        if bytes > MAX_WORD_CONTEXT_BYTES {
            bail!("transcript word context exceeds 512 KiB; analyze a shorter source range");
        }
    }
    Ok(json!({
        "timestamp_origin":"seconds relative to source media playback start, not container timecode",
        "alignment":"original supplied word timestamps; provider alignment can need listening review",
        "content_identity_verified":transcript.usage.as_ref().and_then(|u|u.get("source_sha256")).and_then(Value::as_str).is_some(),
        "range_start":start,"range_end":end,
        "boundary_policy":"overlapping words retain original times, including words crossing range boundaries; do not clip audible words merely to fit this range",
        "words":words,
    }))
}

fn analyzed_range(analysis: &Analysis) -> Result<(f64, f64)> {
    if analysis.version != 1 {
        bail!("unsupported analysis version {}", analysis.version);
    }
    let start = analysis.range["start"]
        .as_f64()
        .context("analysis needs range.start")?;
    let duration = analysis.range["duration"]
        .as_f64()
        .context("analysis needs range.duration")?;
    crate::media::validate_range(&analysis.media, start, duration)?;
    Ok((start, start + duration))
}

pub const DIALOGUE_EDIT_GUIDANCE: &str = "Use supplied original word start/end times to assemble audible dialogue; never estimate word positions from a text summary or sparse frames. Preserve complete thoughts and avoid cutting audible words. source_in may retain subframe word precision; output durations/offsets use the frame grid, so round the consumed source end outward when needed to retain the last word. Words crossing an analyzed boundary are context, not permission to extend past source duration or observed range: choose another complete thought or request a wider analysis. Muted B-roll and intentional J/L cuts can cross spoken words visually when continuous dialogue is carried on a separate audio track. Silence and untranscribed regions are not proven speech-free. Review provider alignment by listening; no automatic transcription or paid retakes are authorized.";

/// Attach explicit existing transcripts, without transcription or API work.
/// Main resolves relative transcript sources against their input file first.
/// Same-window identical word evidence is idempotent; conflicting evidence is
/// rejected rather than silently overriding an embedded or previous transcript.
/// Only bounded relevant words are copied, and all checks precede mutation.
pub fn attach_transcripts(
    analyses: &mut [Analysis],
    transcripts: &[crate::transcript::Transcript],
) -> Result<()> {
    if analyses.len() > 32 || transcripts.len() > 32 {
        bail!("suggest supports at most 32 analyses and 32 transcripts");
    }
    let mut identities = Vec::new();
    for analysis in analyses.iter() {
        analyzed_range(analysis)?;
        let source = crate::media::canonical_file(&analysis.media.path)?;
        let hash = crate::media::file_sha256(&source)?;
        if !analysis.source_sha256.is_empty() && analysis.source_sha256 != hash {
            bail!(
                "analysis source content has changed; analyze the current media before suggesting an edit"
            );
        }
        identities.push((source, hash));
    }
    let mut pending: std::collections::HashMap<usize, crate::transcript::Transcript> =
        std::collections::HashMap::new();
    for transcript in transcripts {
        transcript.validate()?;
        let fingerprint = transcript
            .usage
            .as_ref()
            .and_then(|u| u.get("source_sha256"))
            .map(|hash| {
                hash.as_str()
                    .context("invalid transcript source fingerprint")
            })
            .transpose()?;
        let legacy_source = if fingerprint.is_none() {
            Some(
                crate::media::canonical_file(&transcript.source)
                    .context("resolving transcript source")?,
            )
        } else {
            None
        };
        let mut matched = false;
        for (index, (source, hash)) in identities.iter().enumerate() {
            if fingerprint.is_some_and(|expected| expected != hash)
                || legacy_source
                    .as_ref()
                    .is_some_and(|expected| expected != source)
            {
                continue;
            }
            matched = true;
            let analysis = &analyses[index];
            transcript.verify_source(source, Path::new("."), hash)?;
            let (start, end) = analyzed_range(analysis)?;
            let words = transcript.words_in_range(start, end, MAX_CONTEXT_WORDS)?;
            let mut bytes = 0usize;
            for word in &words {
                bytes += serde_json::to_vec(word)?.len() + 1;
                if bytes > MAX_WORD_CONTEXT_BYTES {
                    bail!(
                        "transcript word context exceeds 512 KiB; analyze a shorter source range"
                    );
                }
            }
            if let Some(existing) = pending.get(&index).or(analysis.transcript.as_ref()) {
                existing.verify_source(source, Path::new("."), hash)?;
                let previous = existing.words_in_range(start, end, MAX_CONTEXT_WORDS)?;
                if serde_json::to_value(previous)? != serde_json::to_value(&words)? {
                    bail!(
                        "conflicting transcripts for analyzed source {}; select one word alignment explicitly",
                        source.display()
                    );
                }
                continue;
            }
            let window = crate::transcript::Transcript {
                version: 1,
                source: source.clone(),
                duration: transcript.duration,
                text: words
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                words: words.into_iter().cloned().collect(),
                model: None,
                usage: fingerprint.map(|_| json!({"source_sha256":hash})),
            };
            pending.insert(index, window);
        }
        if !matched {
            bail!(
                "supplied transcript does not match any analyzed source; supply the original source or an explicitly remapped transcript"
            );
        }
    }
    for (index, transcript) in pending {
        let verified = transcript.usage.is_some();
        analyses[index].transcript = Some(transcript);
        if let Some(coverage) = analyses[index].coverage.as_object_mut() {
            coverage.insert("transcript_available".into(), json!(true));
            coverage.insert(
                "transcript_content_identity_verified".into(),
                json!(verified),
            );
        }
    }
    Ok(())
}

/// The edit consumer rechecks current source identity, including transcripts
/// embedded in saved reports, before exposing a bounded relevant word window.
/// Raw transcript text/usage and unused words never enter the model request.
pub fn suggestion_context(analyses: &[Analysis]) -> Result<Value> {
    if analyses.len() > 32 {
        bail!("suggest supports at most 32 analyses");
    }
    let mut contexts = Vec::new();
    let mut total = 2usize;
    for analysis in analyses {
        analyzed_range(analysis)?;
        let source = crate::media::canonical_file(&analysis.media.path)?;
        let hash = crate::media::file_sha256(&source)?;
        if !analysis.source_sha256.is_empty() && analysis.source_sha256 != hash {
            bail!(
                "analysis source content has changed; analyze the current media before suggesting an edit"
            );
        }
        if let Some(transcript) = &analysis.transcript {
            transcript.verify_source(&source, Path::new("."), &hash)?;
        }
        let context = json!({
            "source":source,"source_sha256":hash,"duration":analysis.media.duration,
            "range":analysis.range,"coverage":analysis.coverage,
            "understanding":analysis.understanding,"dialogue":word_context(analysis)?,
        });
        total = total
            .checked_add(serde_json::to_vec(&context)?.len() + 1)
            .context("analysis context size overflow")?;
        if total > MAX_SUGGEST_CONTEXT_BYTES {
            bail!("combined analysis context exceeds eight MiB; select fewer sources/ranges");
        }
        contexts.push(context);
    }
    let context = json!({"dialogue_edit_guidance":DIALOGUE_EDIT_GUIDANCE,"sources":contexts});
    if serde_json::to_vec(&context)?.len() > MAX_SUGGEST_CONTEXT_BYTES {
        bail!("combined analysis context exceeds eight MiB; select fewer sources/ranges");
    }
    Ok(context)
}

/// The exact edit-suggestion request text; the response cache key depends on it.
pub fn suggest_prompt(brief: &str, edit_schema: &Value, analyses: &str) -> String {
    format!(
        "Create a coherent, well-paced assembly edit following this brief: {brief}\nUse only the explicitly listed analyzed source paths. Respect source durations; do not invent dialogue or unsampled actions. Use verified transcript word spans to keep spoken words intact when selecting dialogue. Intentional B-roll and J/L cuts remain valid. Prefer complete thoughts and purposeful cuts. Place every clip at an explicit timeline start; empty timeline is black and silent. Put continuous dialogue on video track 0 or an audio track and B-roll on video track 1 above it (mute B-roll sound with audio.volume 0 when the dialogue continues underneath). Return ONLY an edit JSON object with \"version\": 2 conforming to this schema: {}\nAll source paths must be ABSOLUTE paths copied from the analyses. Duration is timeline seconds; source consumption = duration*speed, so normal speed=1. Quantize starts and durations to the requested frame grid. Clips on one track must not overlap. No titles, LUTs, source settings or unsupported fields. The default target is 1280x720 at 30/1 fps unless brief requests otherwise. Analyses: {}",
        edit_schema, analyses
    )
}

/// Advisory review for audible cuts only; never rejects deliberate edits.
pub fn dialogue_cut_warnings(
    edit: &crate::timeline::ValidatedEdit,
    analyses: &[Analysis],
) -> Result<Vec<String>> {
    struct Span<'a> {
        label: String,
        source: &'a Path,
        first: f64,
        last: f64,
        offset: f64,
        end: f64,
        speed: f64,
        volume: f64,
    }
    let spans = edit
        .audible_spans()
        .into_iter()
        .map(|span| Span {
            label: span.label,
            source: span.source,
            first: span.source_in,
            last: span.source_in + span.duration * span.speed,
            offset: span.start,
            end: span.start + span.duration,
            speed: span.speed,
            // A cut is only continuous when the gain carries on unchanged.
            volume: if span.mix.keyframes.is_empty()
                && span.mix.fade_in == 0.0
                && span.mix.fade_out == 0.0
            {
                span.mix.volume
            } else {
                f64::NAN
            },
        })
        .collect::<Vec<_>>();
    let near = |a: f64, b: f64| (a - b).abs() <= 1e-6;
    let mut warnings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for analysis in analyses {
        let Some(transcript) = &analysis.transcript else {
            continue;
        };
        let source = crate::media::canonical_file(&analysis.media.path)?;
        let (start, end) = analyzed_range(analysis)?;
        let words = transcript.words_in_range(start, end, MAX_CONTEXT_WORDS)?;
        for span in spans.iter().filter(|span| span.source == source) {
            for (side, boundary, offset) in [
                ("in", span.first, span.offset),
                ("out", span.last, span.end),
            ] {
                let continuous = spans.iter().any(|other| {
                    !std::ptr::eq(span, other)
                        && span.source == other.source
                        && near(span.speed, other.speed)
                        && near(span.volume, other.volume)
                        && if side == "in" {
                            near(other.last, boundary) && near(other.end, offset)
                        } else {
                            near(other.first, boundary) && near(other.offset, offset)
                        }
                });
                if continuous {
                    continue;
                }
                if let Some(word) = words
                    .iter()
                    .find(|word| word.start + 1e-6 < boundary && boundary < word.end - 1e-6)
                {
                    let text = word.text.chars().take(64).collect::<String>();
                    let warning = format!(
                        "Dialogue review: {} audible source {side} at {boundary:.9}s falls inside supplied word {text:?} ({:.9}..{:.9}s); listen and preserve the whole word if this cut is unintended",
                        span.label, word.start, word.end
                    );
                    if seen.insert(warning.clone()) {
                        warnings.push(warning);
                    }
                    if warnings.len() == 128 {
                        warnings.push("Additional dialogue cut warnings omitted; inspect remaining audible boundaries".into());
                        return Ok(warnings);
                    }
                }
            }
        }
    }
    Ok(warnings)
}

fn analysis_prompt(analysis: &Analysis, brief: Option<&str>) -> Result<String> {
    let transcript = word_context(analysis)?;
    Ok(format!("You are helping a human or coding agent make a factual, well-paced edit. Analyze only the supplied source frames, audio sample, and timestamped transcript. Frames are sparse: do not invent motion, dialogue, exact shot boundaries, or unseen events. Source metadata: {}. Analyzed range: {}. Coverage: {}. Transcript words: {}. Dialogue editing guidance: {}. Editorial brief: {}. Return a JSON object with summary (string), observations ([{{time:number,description:string,confidence:string}}]), dialogue_summary (string or null), selects ([{{source_in:number,source_out:number,reason:string,needs_review:boolean}}]), edit_suggestions ([string]), and limitations ([string]). Select ranges must stay within the analyzed time range and are candidate ranges requiring review, not measured shot boundaries. For audio-only observations add the supplied audio sample start to sample-local times. Express all times in source seconds.", {
        let mut metadata = serde_json::to_value(&analysis.media)?;
        if let Some(object) = metadata.as_object_mut() {
            object.remove("path");
            object.insert("source_sha256".into(), json!(analysis.source_sha256));
        }
        metadata.to_string()
    }, analysis.range, analysis.coverage, transcript, DIALOGUE_EDIT_GUIDANCE, brief.unwrap_or("Identify useful material and suggest an assembly with an opening, coherent progression, and ending.")))
}

fn validate_understanding(value: &Value, start: f64, duration: f64) -> Result<()> {
    if !value.is_object() || !value["summary"].is_string() {
        bail!("analysis model must return an object with a summary string");
    }
    for key in ["observations", "selects", "edit_suggestions", "limitations"] {
        if !value[key].is_array() {
            bail!("analysis model returned invalid {key}; expected an array");
        }
    }
    for observation in value["observations"].as_array().unwrap() {
        let time = observation["time"]
            .as_f64()
            .context("observation needs a numeric source time")?;
        if !time.is_finite()
            || time < start
            || time > start + duration
            || !observation["description"].is_string()
        {
            bail!("observation is outside analyzed source range or lacks description");
        }
    }
    for select in value["selects"].as_array().unwrap() {
        let first = select["source_in"]
            .as_f64()
            .context("select needs source_in")?;
        let last = select["source_out"]
            .as_f64()
            .context("select needs source_out")?;
        if !first.is_finite()
            || !last.is_finite()
            || first < start
            || last > start + duration
            || last <= first
            || !select["reason"].is_string()
        {
            bail!("candidate select is outside analyzed source range or invalid");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_suggestions_stay_within_observed_range() {
        let valid = json!({"summary":"Interview","observations":[{"time":11,"description":"Speaker"}],"selects":[{"source_in":10,"source_out":12,"reason":"Complete phrase","needs_review":true}],"edit_suggestions":[],"limitations":[]});
        validate_understanding(&valid, 10.0, 5.0).unwrap();
        let mut invalid = valid.clone();
        invalid["selects"][0]["source_out"] = json!(100);
        assert!(validate_understanding(&invalid, 10.0, 5.0).is_err());
        invalid = valid.clone();
        invalid["observations"][0]["time"] = json!(0);
        assert!(validate_understanding(&invalid, 10.0, 5.0).is_err());
        assert!(validate_understanding(&json!({}), 0.0, 10.0).is_err());
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use std::io::{Read, Write};
    #[test]
    fn identical_media_copies_reuse_frames_and_paid_response_without_stale_paths() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.mp4");
        let copy = dir.path().join("copy.mp4");
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "color=red:s=64x64:d=2:r=10",
                "-c:v",
                "libx264",
            ])
            .arg(&original)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::copy(&original, &copy).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api = crate::openrouter::OpenRouter::for_test(
            format!("http://{}", listener.local_addr().unwrap()),
            Some(dir.path().join("responses")),
            std::time::Duration::from_secs(3),
        );
        let original_text = original.to_string_lossy().to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut headers = Vec::new();
            let mut byte = [0];
            while !headers.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
                assert!(headers.len() < 8192);
            }
            let length = String::from_utf8(headers)
                .unwrap()
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|s| s.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            assert!(length < 64 * 1024);
            let mut request = vec![0; length];
            stream.read_exact(&mut request).unwrap();
            let request = String::from_utf8(request).unwrap();
            assert!(!request.contains(&original_text));
            assert!(request.contains("source_sha256"));
            let body=serde_json::to_vec(&json!({"choices":[{"message":{"content":serde_json::to_string(&json!({"summary":"red","observations":[],"selects":[],"edit_suggestions":[],"limitations":[]})).unwrap()}}],"usage":{"cost":0}})).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        });
        let frames = dir.path().join("assets");
        let options = AnalyzeOptions {
            frames: 2,
            frames_dir: &frames,
            start: 0.0,
            duration: Some(2.0),
            model: Some("test/model"),
            transcript: None,
            prompt: None,
        };
        let first = analyze(
            crate::media::probe(&original).unwrap(),
            Some(&api),
            &options,
        )
        .unwrap();
        server.join().unwrap();
        let second = analyze(crate::media::probe(&copy).unwrap(), Some(&api), &options).unwrap();
        assert!(!first.cached);
        assert!(!first.frames_cached);
        assert!(second.cached);
        assert!(second.frames_cached);
        assert_eq!(first.source_sha256, second.source_sha256);
        assert_eq!(first.frames[0].path, second.frames[0].path);
        assert_eq!(second.media.path, copy.canonicalize().unwrap());
        assert_ne!(first.media.path, second.media.path);
        let different = analyze(
            crate::media::probe(&copy).unwrap(),
            None,
            &AnalyzeOptions {
                start: 0.5,
                duration: Some(1.0),
                ..options
            },
        )
        .unwrap();
        assert_ne!(first.frames[0].path, different.frames[0].path);
        let transcript_file = dir.path().join("transcript.json");
        let mut transcript = json!({"version":1,"source":"/missing/original-machine.mp4","duration":2,"text":"","words":[],"usage":{"source_sha256":second.source_sha256}});
        crate::io::write_json(&transcript_file, &transcript, false).unwrap();
        let transcript_options = AnalyzeOptions {
            transcript: Some(&transcript_file),
            ..options
        };
        let remapped = analyze(
            crate::media::probe(&copy).unwrap(),
            None,
            &transcript_options,
        )
        .unwrap();
        assert_eq!(
            remapped.transcript.unwrap().source,
            copy.canonicalize().unwrap()
        );
        transcript["usage"]["source_sha256"] = json!("0".repeat(64));
        crate::io::write_json(&transcript_file, &transcript, true).unwrap();
        assert!(
            analyze(
                crate::media::probe(&copy).unwrap(),
                None,
                &transcript_options
            )
            .unwrap_err()
            .to_string()
            .contains("content has changed")
        );
        transcript["usage"] = Value::Null;
        transcript["source"] = json!(original);
        crate::io::write_json(&transcript_file, &transcript, true).unwrap();
        assert!(
            analyze(
                crate::media::probe(&copy).unwrap(),
                None,
                &transcript_options
            )
            .unwrap_err()
            .to_string()
            .contains("another media file")
        );
        std::fs::write(&second.frames[0].path, b"bad-cache-data").unwrap();
        assert!(
            analyze(crate::media::probe(&copy).unwrap(), None, &options)
                .unwrap_err()
                .to_string()
                .contains("integrity")
        );
    }
}

#[cfg(test)]
mod dialogue_tests {
    use super::*;

    fn fixture(dir: &Path) -> Analysis {
        let source = dir.join("speech.wav");
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "aevalsrc=0.1*sin(2*PI*(317*t+2*t*t)):s=16000:d=4",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(&source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let media = crate::media::probe(&source).unwrap();
        let hash = crate::media::file_sha256(&source).unwrap();
        // Supplied provider protocol fixture, never inferred from the waveform.
        let transcript = serde_json::from_value(json!({
            "version":1,"source":source,"duration":4.0,"text":"UNBOUNDED SUMMARY MUST NOT ENTER EDIT REQUEST",
            "usage":{"source_sha256":hash},
            "words":[
                {"text":"before","start":0.1,"end":0.3},
                {"text":"crossing-in","start":0.900123,"end":1.200456},
                {"text":"inside","start":1.300123,"end":1.600456},
                {"text":"crossing-out","start":1.800123,"end":2.200456},
                {"text":"after","start":2.300123,"end":2.600456}
            ]
        })).unwrap();
        Analysis {
            version: 1,
            media,
            range: json!({"start":1.0,"duration":1.0}),
            frames: vec![],
            coverage: json!({"visual":"sparse"}),
            transcript: Some(transcript),
            understanding: None,
            model: None,
            usage: None,
            cached: false,
            source_sha256: hash,
            frames_cached: false,
        }
    }

    #[test]
    fn analysis_and_edit_context_preserve_crossing_words_and_exact_original_times() {
        let dir = tempfile::tempdir().unwrap();
        let report = fixture(dir.path());
        let prompt = analysis_prompt(&report, None).unwrap();
        assert!(prompt.contains("crossing-in"));
        assert!(prompt.contains("crossing-out"));
        assert!(prompt.contains("0.900123"));
        assert!(!prompt.contains("UNBOUNDED SUMMARY"));
        let context = suggestion_context(&[report]).unwrap();
        let words = context["sources"][0]["dialogue"]["words"]
            .as_array()
            .unwrap();
        assert_eq!(words.len(), 3);
        assert_eq!(words[0]["start"], 0.900123);
        assert_eq!(words[2]["end"], 2.200456);
        assert!(
            context["dialogue_edit_guidance"]
                .as_str()
                .unwrap()
                .contains("intentional J/L cuts")
        );
        assert!(!context.to_string().contains("UNBOUNDED SUMMARY"));
    }

    #[test]
    fn edit_context_rejects_stale_analysis_and_misbound_embedded_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let mut report = fixture(dir.path());
        report.transcript.as_mut().unwrap().usage = Some(json!({"source_sha256":"0".repeat(64)}));
        assert!(
            suggestion_context(&[report])
                .unwrap_err()
                .to_string()
                .contains("transcript source content has changed")
        );
        let mut report = fixture(dir.path());
        report.source_sha256 = "0".repeat(64);
        assert!(
            suggestion_context(&[report])
                .unwrap_err()
                .to_string()
                .contains("analysis source content has changed")
        );
    }

    #[test]
    fn empty_dialogue_is_valid_and_excess_context_fails_without_silent_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let mut report = fixture(dir.path());
        report.transcript = None;
        assert!(suggestion_context(&[report]).unwrap()["sources"][0]["dialogue"].is_null());
        let mut report = fixture(dir.path());
        let transcript = report.transcript.as_mut().unwrap();
        let mut word = transcript.words[2].clone();
        word.text = "x".repeat(4096);
        transcript.words = vec![word; 130];
        assert!(
            suggestion_context(&[report])
                .unwrap_err()
                .to_string()
                .contains("512 KiB")
        );
        let mut report = fixture(dir.path());
        let transcript = report.transcript.as_mut().unwrap();
        transcript.words = vec![transcript.words[2].clone(); MAX_CONTEXT_WORDS + 1];
        assert!(
            suggestion_context(&[report])
                .unwrap_err()
                .to_string()
                .contains("8192 words")
        );
    }

    #[test]
    fn remote_analysis_request_contains_original_boundary_words() {
        use std::io::{Read, Write};
        let dir = tempfile::tempdir().unwrap();
        let report = fixture(dir.path());
        let words = dir.path().join("words.json");
        crate::io::write_json(&words, report.transcript.as_ref().unwrap(), false).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api = crate::openrouter::OpenRouter::for_test(
            format!("http://{}", listener.local_addr().unwrap()),
            None,
            std::time::Duration::from_secs(3),
        );
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut header = Vec::new();
            let mut byte = [0];
            while !header.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
                assert!(header.len() < 8192);
            }
            let length: usize = String::from_utf8(header)
                .unwrap()
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|s| s.trim().parse().unwrap())
                })
                .unwrap();
            assert!(length < 32 * 1024);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            let request: Value = serde_json::from_slice(&body).unwrap();
            let text = request["messages"][0]["content"][0]["text"]
                .as_str()
                .unwrap();
            assert!(text.contains("crossing-in") && text.contains("crossing-out"));
            assert!(text.contains("0.900123") && text.contains("2.200456"));
            assert!(!text.contains("UNBOUNDED SUMMARY"));
            let body = serde_json::to_vec(&json!({"choices":[{"message":{"content":"{\"summary\":\"dialogue\",\"observations\":[],\"selects\":[],\"edit_suggestions\":[],\"limitations\":[]}"}}]})).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        });
        let frames = dir.path().join("frames");
        let analyzed = analyze(
            report.media,
            Some(&api),
            &AnalyzeOptions {
                frames: 1,
                frames_dir: &frames,
                start: 1.0,
                duration: Some(1.0),
                model: Some("fixture"),
                transcript: Some(&words),
                prompt: None,
            },
        )
        .unwrap();
        assert!(analyzed.understanding.is_some());
        server.join().unwrap();
    }
    #[test]
    fn explicit_transcripts_attach_bounded_windows_and_reject_conflicts_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let mut report = fixture(dir.path());
        let transcript = report.transcript.take().unwrap();
        let mut reports = [report];
        attach_transcripts(&mut reports, std::slice::from_ref(&transcript)).unwrap();
        assert_eq!(reports[0].transcript.as_ref().unwrap().words.len(), 3);
        attach_transcripts(&mut reports, std::slice::from_ref(&transcript)).unwrap();
        let mut conflicting = transcript.clone();
        conflicting.words[2].start = 1.35;
        assert!(
            attach_transcripts(&mut reports, &[conflicting])
                .unwrap_err()
                .to_string()
                .contains("conflicting transcripts")
        );
        assert_eq!(
            reports[0].transcript.as_ref().unwrap().words[1].start,
            1.300123
        );
        let mut empty_report = fixture(dir.path());
        empty_report.transcript = None;
        let mut unchanged = [empty_report];
        let mut wrong = transcript.clone();
        wrong.usage = Some(json!({"source_sha256":"0".repeat(64)}));
        assert!(attach_transcripts(&mut unchanged, &[transcript.clone(), wrong.clone()]).is_err());
        assert!(unchanged[0].transcript.is_none());
        let mut wrong = transcript.clone();
        wrong.usage = Some(json!({"source_sha256":"0".repeat(64)}));
        assert!(
            attach_transcripts(&mut reports, &[wrong])
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        reports[0].transcript = None;
        let mut legacy = transcript.clone();
        legacy.usage = None;
        attach_transcripts(&mut reports, &[legacy]).unwrap();
        let context = suggestion_context(&reports).unwrap();
        assert_eq!(
            context["sources"][0]["dialogue"]["content_identity_verified"],
            false
        );
    }

    #[test]
    fn audible_word_cut_warnings_cover_unsafe_safe_muted_and_contiguous_audio() {
        let dir = tempfile::tempdir().unwrap();
        let mut report = fixture(dir.path());
        report.range = json!({"start":0.0,"duration":4.0});
        let words = &mut report.transcript.as_mut().unwrap().words;
        words[2].start = 1.3;
        words[2].end = 1.6;
        let source = report.media.path.clone();
        let validate = |audio: Value| {
            let edit = crate::timeline::parse(
                json!({"version":2,"duration":1.0,"audio":[{"clips":audio}]}),
            )
            .unwrap();
            crate::timeline::validate(&edit, dir.path()).unwrap()
        };
        let unsafe_edit =
            validate(json!([{"start":0,"source":source,"source_in":1.4,"duration":0.1}]));
        assert_eq!(
            dialogue_cut_warnings(&unsafe_edit, std::slice::from_ref(&report))
                .unwrap()
                .len(),
            2
        );
        let safe = validate(json!([{"start":0,"source":source,"source_in":1.3,"duration":0.3}]));
        assert!(
            dialogue_cut_warnings(&safe, std::slice::from_ref(&report))
                .unwrap()
                .is_empty()
        );
        let muted = validate(
            json!([{"start":0,"source":source,"source_in":1.4,"duration":0.1,"volume":0.0}]),
        );
        assert!(
            dialogue_cut_warnings(&muted, std::slice::from_ref(&report))
                .unwrap()
                .is_empty()
        );
        let contiguous = validate(
            json!([{"start":0,"source":source,"source_in":1.3,"duration":0.1},{"start":0.1,"source":source,"source_in":1.4,"duration":0.2}]),
        );
        assert!(
            dialogue_cut_warnings(&contiguous, std::slice::from_ref(&report))
                .unwrap()
                .is_empty()
        );

        let video = dir.path().join("broll.mp4");
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "color=black:s=64x64:r=30:d=4",
                "-i",
            ])
            .arg(&source)
            .args(["-c:v", "libx264", "-c:a", "aac", "-shortest"])
            .arg(&video)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        report.media = crate::media::probe(&video).unwrap();
        report.source_sha256 = crate::media::file_sha256(&video).unwrap();
        report.transcript.as_mut().unwrap().source = video.clone();
        report.transcript.as_mut().unwrap().usage =
            Some(json!({"source_sha256":report.source_sha256}));
        let edit = crate::timeline::parse(
            json!({"version":2,"video":[{"clips":[{"start":0,"source":video,"source_in":1.4,"duration":1.0,"audio":{"volume":0.0}}]}]}),
        )
        .unwrap();
        let muted_broll = crate::timeline::validate(&edit, dir.path()).unwrap();
        assert!(
            dialogue_cut_warnings(&muted_broll, std::slice::from_ref(&report))
                .unwrap()
                .is_empty()
        );
        let edit = crate::timeline::parse(
            json!({"version":2,"video":[{"clips":[{"start":0,"source":video,"source_in":1.4,"duration":0.1}]}]}),
        )
        .unwrap();
        let audible = crate::timeline::validate(&edit, dir.path()).unwrap();
        assert_eq!(
            dialogue_cut_warnings(&audible, std::slice::from_ref(&report))
                .unwrap()
                .len(),
            2
        );
        let edit = crate::timeline::parse(json!({"version":2,"video":[{"clips":[{"start":0,"source":video,"source_in":1.3,"duration":0.1},{"start":0.1,"source":video,"source_in":1.4,"duration":0.2}]}]})).unwrap();
        let continuous_picture_cuts = crate::timeline::validate(&edit, dir.path()).unwrap();
        assert!(
            dialogue_cut_warnings(&continuous_picture_cuts, &[report])
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn aggregate_edit_context_accepts_exact_eight_mib_and_rejects_first_excess_byte() {
        let dir = tempfile::tempdir().unwrap();
        let mut report = fixture(dir.path());
        report.transcript = None;
        report.understanding = Some(json!({"payload":""}));
        let base = serde_json::to_vec(&suggestion_context(std::slice::from_ref(&report)).unwrap())
            .unwrap()
            .len();
        let filler = MAX_SUGGEST_CONTEXT_BYTES - base;
        report.understanding = Some(json!({"payload":"x".repeat(filler)}));
        let exact = suggestion_context(std::slice::from_ref(&report)).unwrap();
        assert_eq!(
            serde_json::to_vec(&exact).unwrap().len(),
            MAX_SUGGEST_CONTEXT_BYTES
        );
        report.understanding = Some(json!({"payload":"x".repeat(filler + 1)}));
        assert!(
            suggestion_context(&[report])
                .unwrap_err()
                .to_string()
                .contains("eight MiB")
        );
    }
}
