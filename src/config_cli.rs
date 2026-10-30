//! Offline configuration commands using the same selection logic as media tasks.
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Args)]
#[command(
    after_help = "Examples:\n  skywrite defaults image\n  skywrite defaults image set PROVIDER/MODEL\n  skywrite defaults music unset\n  skywrite models list --task image"
)]
pub struct DefaultsArgs {
    /// Task or alias: image, video, speech, music; omit to inspect all tasks.
    #[arg(value_name = "TASK")]
    pub task: Option<String>,
    #[command(subcommand)]
    pub action: Option<DefaultAction>,
}

#[derive(Debug, Subcommand)]
pub enum DefaultAction {
    /// Read the saved and effective model, including environment overrides.
    Get,
    /// Save a model ID without contacting or paying its provider.
    Set { model: String },
    /// Remove the saved preference; environment overrides still apply.
    Unset,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Show the merge without creating or changing the personal config.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StarterModels {
    version: u32,
    curated_on: String,
    note: String,
    presets: Vec<StarterModel>,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct StarterModel {
    task: String,
    model: Option<String>,
    reason: String,
}

fn starters() -> Result<StarterModels> {
    let models: StarterModels =
        serde_json::from_str(include_str!("../assets/default-models.json"))?;
    if models.version != 1 || models.presets.len() != crate::models::TASKS.len() {
        bail!("invalid packaged starter model set");
    }
    let mut seen = std::collections::BTreeSet::new();
    for preset in &models.presets {
        crate::models::canonical_task(&preset.task)?;
        if !seen.insert(&preset.task) {
            bail!("duplicate packaged starter model task");
        }
    }
    Ok(models)
}

fn describe(task: &str, saved: &BTreeMap<String, String>) -> Result<Value> {
    let task = crate::models::canonical_task(task)?;
    let mut value = match crate::models::selection_with_defaults(task, None, saved) {
        Ok(selection) => serde_json::to_value(selection)?,
        Err(error) => json!({
            "task":task,"model":null,"source":"invalid_override",
            "saved_model":saved.get(task),"saved_inspected":true,
            "environment_variable":crate::models::task_environment(task)?,
            "selection_error":error.to_string(),
            "note":"A model environment override is invalid. The saved preference is intact; correct or unset the override before running a paid task."
        }),
    };
    value["availability"] = json!("unchecked; use models list to inspect the current free catalog");
    value["available_models_command"] = json!(format!("skywrite models list --task {task}"));
    value["set_command"] = json!(format!("skywrite defaults {task} set MODEL"));
    if task == "image-upscaling" {
        value["configured_model"] = value["model"].clone();
        value["model"] = Value::Null;
        value["source"] = json!("local_tool");
        value["note"] = json!(
            "skywrite upscale image uses local FFmpeg Lanczos resizing. A saved image-upscaling model is reserved and is not used by this command."
        );
    }
    Ok(value)
}

pub fn execute_defaults(args: DefaultsArgs) -> Result<Value> {
    let task = args
        .task
        .as_deref()
        .map(crate::models::canonical_task)
        .transpose()?;
    if args.action.is_some() && task.is_none() {
        bail!("provide a task, for example skywrite defaults image set MODEL");
    }
    let mutation = match args.action {
        Some(DefaultAction::Set { model }) => Some(crate::workspace::set_default_model(
            task.context("missing task")?,
            &model,
        )?),
        Some(DefaultAction::Unset) => Some(crate::workspace::unset_default_model(
            task.context("missing task")?,
        )?),
        Some(DefaultAction::Get) | None => None,
    };
    let saved = crate::workspace::model_defaults()?;
    let config_path = crate::workspace::personal_home()?.join("config.json");
    let mut result = match task {
        Some(task) => describe(task, &saved)?,
        None => json!({
            "defaults":crate::models::TASKS.iter().map(|task| describe(task, &saved)).collect::<Result<Vec<_>>>()?,
            "task_aliases":{"image":"image-generation","video":"video-generation","speech":"speech-generation","music":"music-generation"}
        }),
    };
    result["config_path"] = json!(config_path);
    result["paid_requests"] = json!(false);
    result["network_requests"] = json!(false);
    result["precedence"] = json!([
        "explicit --model",
        "task environment variable",
        "legacy vision environment variable for analysis/editing",
        "saved task model",
        "built-in transcription fallback"
    ]);
    if let Some(mutation) = mutation {
        result["saved_change"] = mutation;
    }
    Ok(result)
}

pub fn initialize(args: InitArgs) -> Result<Value> {
    let starter = starters()?;
    let choices: BTreeMap<String, String> = starter
        .presets
        .iter()
        .filter_map(|preset| {
            preset
                .model
                .as_ref()
                .map(|model| (preset.task.clone(), model.clone()))
        })
        .collect();
    let mut result = crate::workspace::initialize_default_models(&choices, args.dry_run)?;
    let saved: BTreeMap<String, String> = serde_json::from_value(result["model_defaults"].clone())?;
    result["defaults"] = json!(
        crate::models::TASKS
            .iter()
            .map(|task| describe(task, &saved))
            .collect::<Result<Vec<_>>>()?
    );
    result["starter_models"] = serde_json::to_value(starter.presets)?;
    result["curated_on"] = json!(starter.curated_on);
    result["note"] = json!(starter.note);
    result["paid_requests"] = json!(false);
    result["network_requests"] = json!(false);
    result["next_steps"] = json!([
        "Set OPENROUTER_API_KEY in the environment inherited by your agent; no API key is stored by init.",
        "Run skywrite defaults image and skywrite models list --task image-generation to inspect a preference and available models.",
        "Run skywrite defaults image set MODEL to change that saved preference.",
        "Use --model for a single command, or unset the task environment variable if it overrides the saved choice."
    ]);
    Ok(result)
}
