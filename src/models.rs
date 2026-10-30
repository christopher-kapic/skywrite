//! Free, cached model discovery and explicit task model selection.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const TTL: u64 = 24 * 60 * 60;
const MAX_CATALOG: u64 = 8 * 1024 * 1024;
pub const TASKS: &[&str] = crate::workspace::MODEL_TASKS;

/// Canonical task names are persisted; short aliases are accepted at CLI boundaries.
pub fn canonical_task(task: &str) -> Result<&'static str> {
    let name = match task {
        "image" => "image-generation",
        "video" => "video-generation",
        "speech" => "speech-generation",
        "music" => "music-generation",
        other => other,
    };
    TASKS
        .iter()
        .copied()
        .find(|candidate| *candidate == name)
        .with_context(|| {
            format!(
                "unknown model task {task}; choose {} (aliases: image, video, speech, music)",
                TASKS.join(", ")
            )
        })
}
pub fn validate_task(task: &str) -> Result<()> {
    canonical_task(task).map(|_| ())
}
pub fn task_parser() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(TASKS.iter().map(|task| {
        let value = clap::builder::PossibleValue::new(*task);
        match *task {
            "image-generation" => value.alias("image"),
            "video-generation" => value.alias("video"),
            "speech-generation" => value.alias("speech"),
            "music-generation" => value.alias("music"),
            _ => value,
        }
    }))
}
pub fn task_environment(task: &str) -> Result<String> {
    let task = canonical_task(task)?;
    Ok(format!(
        "SKYWRITE_{}_MODEL",
        task.replace('-', "_").to_uppercase()
    ))
}

#[derive(Debug, Serialize)]
pub struct ModelSelection {
    pub task: String,
    pub model: Option<String>,
    pub source: &'static str,
    pub environment_variable: String,
    pub legacy_environment_variable: Option<&'static str>,
    pub saved_model: Option<String>,
    pub saved_inspected: bool,
}

fn selected_model(
    task: &'static str,
    explicit: Option<&str>,
    saved_model: Option<String>,
    saved_inspected: bool,
) -> Result<ModelSelection> {
    let environment_variable = task_environment(task)?;
    let legacy_environment_variable =
        matches!(task, "analysis" | "editing").then_some("OPENROUTER_VISION_MODEL");
    let task_environment = std::env::var(&environment_variable)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let legacy = legacy_environment_variable
        .and_then(|name| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty());
    let (model, source) = if let Some(model) = explicit {
        (Some(model.to_owned()), "explicit")
    } else if let Some(model) = task_environment {
        (Some(model), "environment")
    } else if let Some(model) = legacy {
        (Some(model), "legacy_environment")
    } else if let Some(model) = saved_model.as_ref() {
        (Some(model.clone()), "saved")
    } else if task == "transcription" {
        (Some("openai/whisper-large-v3".into()), "builtin")
    } else {
        (None, "unset")
    };
    if let Some(model) = &model
        && (model.is_empty()
            || model.len() > 256
            || model
                .chars()
                .any(|character| character.is_whitespace() || character.is_control()))
    {
        bail!("model ID must contain 1..256 UTF-8 bytes without whitespace or controls");
    }
    Ok(ModelSelection {
        task: task.into(),
        model,
        source,
        environment_variable,
        legacy_environment_variable,
        saved_model,
        saved_inspected,
    })
}

/// Select exactly as runtime commands do. Higher-precedence overrides do not
/// depend on reading an unused personal config (including a malformed config).
pub fn selection(task: &str, explicit: Option<&str>) -> Result<ModelSelection> {
    let task = canonical_task(task)?;
    let higher = selected_model(task, explicit, None, false)?;
    if matches!(
        higher.source,
        "explicit" | "environment" | "legacy_environment"
    ) {
        return Ok(higher);
    }
    selected_model(
        task,
        explicit,
        crate::workspace::get_default_model(task)?,
        true,
    )
}

/// Configuration inspection loads one consistent config snapshot, but uses the
/// identical selection function and precedence as a real media command.
pub fn selection_with_defaults(
    task: &str,
    explicit: Option<&str>,
    saved: &BTreeMap<String, String>,
) -> Result<ModelSelection> {
    let task = canonical_task(task)?;
    selected_model(task, explicit, saved.get(task).cloned(), true)
}

/// No automatic "best model" substitution; a missing model is actionable.
pub fn resolve_model(task: &str, explicit: Option<&str>) -> Result<String> {
    let selection = selection(task, explicit)?;
    selection.model.with_context(|| format!(
        "{} requires --model, {}, or skywrite defaults {} set MODEL (skywrite init can add starter preferences)",
        selection.task, selection.environment_variable, selection.task
    ))
}
#[derive(Default)]
pub struct ListOptions<'a> {
    pub task: Option<&'a str>,
    pub input: Option<&'a str>,
    pub output: Option<&'a str>,
    pub search: Option<&'a str>,
    pub refresh: bool,
    pub offline: bool,
    pub cache_dir: Option<&'a Path>,
}
#[derive(Serialize, Deserialize)]
struct CatalogCache {
    version: u32,
    endpoint: String,
    fetched_at: u64,
    body: Value,
}
fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
fn load_catalog(options: &ListOptions<'_>, endpoint: &str) -> Result<(Value, Value)> {
    let root = crate::cache::cache_root(options.cache_dir)?;
    let key = crate::openrouter::cache_key(&[b"catalog-v1", endpoint.as_bytes()]);
    let path = root.join("models").join(format!("{key}.json"));
    let cached = if path.exists() && !options.refresh {
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() > MAX_CATALOG {
            bail!("model cache is not a bounded regular file");
        }
        Some(
            crate::io::read_json::<CatalogCache>(&path)
                .context("invalid model catalog cache; use --refresh after removing it")?,
        )
    } else {
        None
    };
    let time = now()?;
    if let Some(cache) = cached.as_ref()
        && cache.version == 1
        && cache.endpoint == endpoint
        && cache.body["data"].is_array()
        && (options.offline
            || (!options.refresh
                && cache.fetched_at <= time
                && time.saturating_sub(cache.fetched_at) < TTL))
    {
        return Ok((
            cache.body.clone(),
            json!({"endpoint":endpoint,"cached":true,"fetched_at":cache.fetched_at,"stale":time.saturating_sub(cache.fetched_at)>=TTL}),
        ));
    }
    if options.offline {
        bail!("no usable offline model catalog for {endpoint}; run models list online first");
    }
    let body = crate::openrouter::OpenRouter::new(None)?.catalog(endpoint)?;
    let cache = CatalogCache {
        version: 1,
        endpoint: endpoint.into(),
        fetched_at: time,
        body: body.clone(),
    };
    let encoded = serde_json::to_vec(&cache)?;
    if encoded.len() as u64 > MAX_CATALOG {
        bail!("model catalog cache exceeds 8 MiB");
    }
    crate::io::atomic_write(&path, &encoded, true)?;
    Ok((
        body,
        json!({"endpoint":endpoint,"cached":false,"fetched_at":time,"stale":false}),
    ))
}
fn has_modality(model: &Value, side: &str, wanted: &str) -> bool {
    model["architecture"][format!("{side}_modalities")]
        .as_array()
        .is_some_and(|items| items.iter().any(|v| v.as_str() == Some(wanted)))
}
fn normalize(mut value: Value, api: &str, generic: Option<&Value>) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("api".into(), json!(api));
        if let Some(generic) = generic {
            for key in [
                "architecture",
                "pricing",
                "context_length",
                "supported_parameters",
            ] {
                if !object.contains_key(key) && !generic[key].is_null() {
                    object.insert(key.into(), generic[key].clone());
                }
            }
        }
        if api == "videos" {
            let upscale = object.get("upscale_factor").is_some_and(|v| !v.is_null());
            object.insert(
                "task".into(),
                json!(if upscale {
                    "video-upscaling"
                } else {
                    "video-generation"
                }),
            );
            if !object.contains_key("architecture") {
                object.insert(
                    "architecture".into(),
                    json!({"input_modalities":[],"output_modalities":["video"]}),
                );
            }
            object.insert("pricing_note".into(),json!("Use pricing_skus with their explicit units. Generic prompt/completion zero prices do not mean free video. Input-reference/audio fees may apply. Unknown or absent prices are not zero."));
        }
        if api == "images" && object.get("pricing").is_none() {
            object.insert("pricing_note".into(),json!("Price unavailable in this catalog; do not assume zero. Check model endpoint pricing before generation."));
        }
    }
    value
}
pub fn list(options: &ListOptions<'_>) -> Result<Value> {
    if options.offline && options.refresh {
        bail!("--offline and --refresh cannot be combined");
    }
    if options.search.is_some_and(|s| s.len() > 256) {
        bail!("model search exceeds 256 bytes");
    }
    let mut sources = Vec::new();
    let mut data = Vec::new();
    let task = options.task.map(canonical_task).transpose()?;
    if task.is_none() || matches!(task, Some("analysis" | "editing")) {
        let (catalog, metadata) = load_catalog(options, "models")?;
        sources.push(metadata);
        data.extend(
            catalog["data"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| {
                    has_modality(v, "output", "text")
                        && (task != Some("analysis")
                            || has_modality(v, "input", "image")
                            || has_modality(v, "input", "audio"))
                })
                .cloned()
                .map(|v| normalize(v, "chat/completions", None)),
        );
    }
    if task.is_none() || task == Some("transcription") {
        let (catalog, metadata) = load_catalog(options, "models?output_modalities=transcription")?;
        sources.push(metadata);
        data.extend(
            catalog["data"]
                .as_array()
                .unwrap()
                .iter()
                .cloned()
                .map(|v| normalize(v, "audio/transcriptions", None)),
        );
    }
    if task.is_none() || task == Some("speech-generation") {
        let (catalog, metadata) = load_catalog(options, "models?output_modalities=speech")?;
        sources.push(metadata);
        data.extend(catalog["data"].as_array().unwrap().iter().cloned().map(|mut value| {
            value["task"] = json!("speech-generation");
            value["pricing_note"] = json!("Speech prices can be per input character or generated second; consult the selected model. Voice identifiers and output formats differ by provider.");
            normalize(value, "audio/speech", None)
        }));
    }
    if task.is_none() || task == Some("music-generation") {
        let (catalog, metadata) = load_catalog(options, "models?output_modalities=audio")?;
        sources.push(metadata);
        data.extend(catalog["data"].as_array().unwrap().iter().filter(|value| {
            value["id"].as_str().is_some_and(|id| id.starts_with("google/lyria-"))
        }).cloned().map(|mut value| {
            value["task"] = json!("music-generation");
            value["pricing_note"] = json!("Music is billed per generated song/clip. Zero generic token prices do NOT mean free music; check the model's song price. Prompted duration, instrumentation and vocals are requests, not guarantees.");
            normalize(value, "chat/completions", None)
        }));
    }
    if task.is_none() || matches!(task, Some("image-generation" | "image-upscaling")) {
        let (catalog, metadata) = load_catalog(options, "images/models")?;
        sources.push(metadata);
        let (prices, metadata) = load_catalog(options, "models?output_modalities=image")?;
        sources.push(metadata);
        let generic: BTreeMap<_, _> = prices["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v["id"].as_str().map(|id| (id, v)))
            .collect();
        data.extend(
            catalog["data"]
                .as_array()
                .unwrap()
                .iter()
                .cloned()
                .map(|v| {
                    let extra = v["id"].as_str().and_then(|id| generic.get(id)).copied();
                    normalize(v, "images", extra)
                })
                .filter(|v| {
                    task != Some("image-upscaling")
                        || v["upscale_factor"].is_object()
                        || v["supported_parameters"]["upscale_factor"].is_object()
                        || v["id"].as_str().is_some_and(|id| id.contains("upscale"))
                }),
        );
    }
    if task.is_none() || matches!(task, Some("video-generation" | "video-upscaling")) {
        let (catalog, metadata) = load_catalog(options, "videos/models")?;
        sources.push(metadata);
        let (generic, metadata) = load_catalog(options, "models?output_modalities=video")?;
        sources.push(metadata);
        let generic: BTreeMap<_, _> = generic["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v["id"].as_str().map(|id| (id, v)))
            .collect();
        data.extend(
            catalog["data"]
                .as_array()
                .unwrap()
                .iter()
                .cloned()
                .map(|v| {
                    let extra = v["id"].as_str().and_then(|id| generic.get(id)).copied();
                    normalize(v, "videos", extra)
                })
                .filter(|v| task.is_none() || v["task"].as_str() == task),
        );
    }
    data.retain(|v| {
        options.input.is_none_or(|m| has_modality(v, "input", m))
            && options.output.is_none_or(|m| has_modality(v, "output", m))
            && options.search.is_none_or(|s| {
                ["id", "name", "description"].iter().any(|k| {
                    v[k].as_str()
                        .is_some_and(|t| t.to_lowercase().contains(&s.to_lowercase()))
                })
            })
    });
    data.sort_by(|a, b| {
        a["id"]
            .as_str()
            .cmp(&b["id"].as_str())
            .then(a["api"].as_str().cmp(&b["api"].as_str()))
    });
    let warnings = if task == Some("image-upscaling") && data.is_empty() {
        vec![
            "No dedicated image-upscaling API model is currently cataloged. Higher-resolution generative image editing can change details and is not faithful upscaling.",
        ]
    } else {
        vec![]
    };
    Ok(
        json!({"version":1,"task":task,"models":data,"sources":sources,"warnings":warnings,"catalog_ttl_seconds":TTL,"paid_inference_performed":false,"note":"Catalog capabilities and prices are provider claims, not successful compatibility tests. Word timestamp support is not guaranteed by transcription modality alone."}),
    )
}

/// Find a task-compatible model from its real dedicated catalog, never a paid call.
pub fn find_model(task: &str, id: &str, cache_dir: Option<&Path>, offline: bool) -> Result<Value> {
    let catalog = list(&ListOptions {
        task: Some(task),
        cache_dir,
        offline,
        ..Default::default()
    })?;
    catalog["models"].as_array().unwrap().iter().find(|v|v["id"].as_str()==Some(id)).cloned()
        .with_context(||format!("model {id} is not in the {task} catalog; refresh discovery or choose a listed model"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dedicated_video_pricing_and_upscale_survive() {
        let value = normalize(
            json!({"id":"vendor/up","upscale_factor":{"min":1.5,"max":3},"pricing_skus":{"cents_per_megapixel_second_precise":"3.5"}}),
            "videos",
            None,
        );
        assert_eq!(value["task"], "video-upscaling");
        assert_eq!(
            value["pricing_skus"]["cents_per_megapixel_second_precise"],
            "3.5"
        );
        assert!(
            value["pricing_note"]
                .as_str()
                .unwrap()
                .contains("do not mean free")
        );
    }
    #[test]
    fn explicit_model_wins_and_tasks_are_finite() {
        assert_eq!(
            resolve_model("analysis", Some("vendor/model")).unwrap(),
            "vendor/model"
        );
        assert!(resolve_model("fake", Some("vendor/model")).is_err());
        assert!(resolve_model("analysis", Some("bad\nmodel")).is_err());
        assert_eq!(
            task_environment("video-generation").unwrap(),
            "SKYWRITE_VIDEO_GENERATION_MODEL"
        );
    }
}
