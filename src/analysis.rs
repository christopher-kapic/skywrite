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
    if let Some(transcript) = &transcript
        && transcript
            .usage
            .as_ref()
            .and_then(|u| u.get("source_sha256"))
            .and_then(Value::as_str)
            .is_none()
    {
        let source_path = if transcript.source.is_absolute() {
            transcript.source.clone()
        } else {
            options
                .transcript
                .and_then(Path::parent)
                .unwrap_or(Path::new("."))
                .join(&transcript.source)
        };
        let source =
            crate::media::canonical_file(&source_path).context("resolving transcript source")?;
        if source != info.path {
            bail!(
                "transcript belongs to another media file; first remap timestamps explicitly using sync results"
            );
        }
    }
    if let Some(expected) = transcript
        .as_ref()
        .and_then(|t| t.usage.as_ref())
        .and_then(|u| u.get("source_sha256"))
        .and_then(Value::as_str)
        && source_sha256 != expected
    {
        bail!(
            "transcript source content has changed; transcribe the current media before analyzing"
        );
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
            vec![json!({"type":"text","text":analysis_prompt(&analysis, options.prompt)})];
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
fn analysis_prompt(analysis: &Analysis, brief: Option<&str>) -> String {
    let transcript = analysis
        .transcript
        .as_ref()
        .map(|t| {
            let start = analysis.range["start"].as_f64().unwrap_or(0.0);
            let end = start + analysis.range["duration"].as_f64().unwrap_or(0.0);
            serde_json::to_string(
                &t.words
                    .iter()
                    .filter(|w| w.start >= start && w.end <= end)
                    .collect::<Vec<_>>(),
            )
        })
        .transpose()
        .ok()
        .flatten()
        .unwrap_or_else(|| "not provided".to_owned());
    format!("You are helping a human or coding agent make a factual, well-paced edit. Analyze only the supplied source frames, audio sample, and timestamped transcript. Frames are sparse: do not invent motion, dialogue, exact shot boundaries, or unseen events. Source metadata: {}. Analyzed range: {}. Coverage: {}. Transcript words: {}. Editorial brief: {}. Return a JSON object with summary (string), observations ([{{time:number,description:string,confidence:string}}]), dialogue_summary (string or null), selects ([{{source_in:number,source_out:number,reason:string,needs_review:boolean}}]), edit_suggestions ([string]), and limitations ([string]). Select ranges must stay within the analyzed time range and are candidate ranges requiring review, not measured shot boundaries. For audio-only observations add the supplied audio sample start to sample-local times. Express all times in source seconds.", {
        let mut metadata = serde_json::to_value(&analysis.media).unwrap_or_default();
        if let Some(object) = metadata.as_object_mut() {
            object.remove("path");
            object.insert("source_sha256".into(), json!(analysis.source_sha256));
        }
        metadata.to_string()
    }, analysis.range, analysis.coverage, transcript, brief.unwrap_or("Identify useful material and suggest an assembly with an opening, coherent progression, and ending."))
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
