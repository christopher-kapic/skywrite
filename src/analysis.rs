use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

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
    let transcript = options
        .transcript
        .map(crate::transcript::Transcript::load)
        .transpose()?;
    if let Some(transcript) = &transcript {
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
        && crate::media::file_sha256(&info.path)? != expected
    {
        bail!(
            "transcript source content has changed; transcribe the current media before analyzing"
        );
    }
    // Each run owns a new frame subdirectory. Repeated analysis never replaces
    // frames referenced by another report or unrelated files in the parent.
    std::fs::create_dir_all(options.frames_dir)?;
    let parent = options.frames_dir.canonicalize()?;
    let run_dir = tempfile::Builder::new()
        .prefix("sample-")
        .tempdir_in(parent)?;
    let frames = crate::media::sample_frames(
        &info,
        run_dir.path(),
        options.frames,
        options.start,
        duration,
    )?;
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
    let _persisted_dir: PathBuf = run_dir.keep();
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
    format!("You are helping a human or coding agent make a factual, well-paced edit. Analyze only the supplied source frames, audio sample, and timestamped transcript. Frames are sparse: do not invent motion, dialogue, exact shot boundaries, or unseen events. Source metadata: {}. Analyzed range: {}. Coverage: {}. Transcript words: {}. Editorial brief: {}. Return a JSON object with summary (string), observations ([{{time:number,description:string,confidence:string}}]), dialogue_summary (string or null), selects ([{{source_in:number,source_out:number,reason:string,needs_review:boolean}}]), edit_suggestions ([string]), and limitations ([string]). Select ranges must stay within the analyzed time range and are candidate ranges requiring review, not measured shot boundaries. For audio-only observations add the supplied audio sample start to sample-local times. Express all times in source seconds.", serde_json::to_string(&analysis.media).unwrap_or_default(), analysis.range, analysis.coverage, transcript, brief.unwrap_or("Identify useful material and suggest an assembly with an opening, coherent progression, and ending."))
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
