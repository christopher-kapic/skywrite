use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "skywrite",
    version,
    about = "Agent-friendly media understanding, synchronization, and editing",
    after_help = "All successful commands emit JSON to stdout; diagnostics go to stderr. Start with skywrite init, then skywrite schema for the layered edit format.\n\nBefore creative choices (music, B-roll, pacing, color), check skywrite styles list for the user's saved preferences; ask the user when none apply.\n\nChoosing cuts: transcribe for word timestamps, survey with skywrite describe frames SOURCE --every 30, then focus in around each candidate with --around TIME --window 1 (or skywrite review sheet). Audition with skywrite review cuts and check with skywrite review lint.\n\nCompose tools: render HTML graphics as image clips on upper video tracks in FCPXML/Premiere/FFmpeg/Hyperframes/Remotion edits; generate character images and reference frames before video generation.\n\nAgents: record ALL frustrations, bugs and missing features with skywrite nit <description>. When Skywrite cannot decode or verify something (camera color metadata, an unsupported format, an NLE import you could not check), include the raw inspect/command output, the camera or app and its operator-confirmed settings, and the relevant source file from https://github.com/christopher-kapic/skywrite (for example src/color.rs or src/export/xmeml.rs)."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Check installed media tools without accessing the network.
    Doctor,
    /// Inspect local media metadata using ffprobe.
    #[command(alias = "ingest")]
    Inspect {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Decode and cache exact first-audio timing instead of trusting metadata.
        #[arg(long)]
        exact_audio: bool,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        #[command(flatten)]
        output: OutputOptions,
    },
    /// Verify local metadata, decoded counts, duration and audio levels.
    Verify(skywrite::verify::VerifyArgs),
    /// Sample timestamped frames and optionally request structured model understanding.
    Analyze(AnalyzeArgs),
    /// Transcribe bounded audio chunks through OpenRouter with real word timestamps.
    Transcribe(TranscribeArgs),
    /// Estimate asset-to-reference time offset and clock scale.
    Sync(SyncArgs),
    /// Validate an edit, export a composition, or render using FFmpeg.
    Edit {
        #[command(subcommand)]
        command: EditCommand,
    },
    /// Discover models and save explicit task defaults (no API key stored).
    Models {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Inspect or set effective task model defaults.
    #[command(alias = "default")]
    Defaults(skywrite::config_cli::DefaultsArgs),
    /// Create offline starter configuration without replacing preferences.
    Init(skywrite::config_cli::InitArgs),
    /// Save/read reusable creative prompts in ~/.skywrite/styles.
    Styles {
        #[command(subcommand)]
        command: skywrite::workspace::ContentCommand,
    },
    /// Read built-in workflows and save personal procedural instructions.
    Skills {
        #[command(subcommand)]
        command: skywrite::workspace::ContentCommand,
    },
    /// Save ALL agent frustrations in ~/.skywrite/nits (never uploads). Include raw output and source file references when something could not be decoded or verified.
    Nit { description: String },
    /// Generate images, videos, narrated speech, or music with durable job records.
    Generate {
        #[command(subcommand)]
        command: skywrite::generation::GenerateCommand,
    },
    /// Inspect or resume generated media without repeating a paid submission.
    Jobs {
        #[command(subcommand)]
        command: skywrite::generation::JobsCommand,
    },
    /// Captions: SRT is the default deliverable (editable as text in NLEs); burn-in or image captions only on request.
    Captions {
        #[command(subcommand)]
        command: CaptionsCommand,
    },
    /// Finish local audio with bounded normalization, tempo, looping and ducking.
    Audio {
        #[command(subcommand)]
        command: skywrite::audio::AudioCommand,
    },
    /// Run or resume a bounded declarative generation project.
    #[command(alias = "batch")]
    Project {
        #[command(subcommand)]
        command: skywrite::project::ProjectCommand,
    },
    /// Dedicated AI video upscaling or explicit non-AI local image resizing.
    Upscale {
        #[command(subcommand)]
        command: skywrite::generation::UpscaleCommand,
    },
    /// Configure public S3/R2 uploads and purge only Skywrite upload objects.
    Storage {
        #[command(subcommand)]
        command: skywrite::storage::StorageCommand,
    },
    /// Share and discover finished assets across machines using S3/R2.
    Outputs {
        #[command(subcommand)]
        command: skywrite::outputs::OutputsCommand,
    },
    /// Model descriptions of exact frames (cached per frame) or short video ranges.
    Describe {
        #[command(subcommand)]
        command: skywrite::describe::DescribeCommand,
    },
    /// Review aids: exact-frame contact sheets, cut audition clips, and objective edit checks.
    Review {
        #[command(subcommand)]
        command: skywrite::review::ReviewCommand,
    },
    /// Print agent-facing edit and transcript schemas/examples.
    Schema,
    /// Render still assets such as title cards using embedded upstream Takumi.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
}
#[derive(Subcommand)]
enum CaptionsCommand {
    Convert {
        transcript: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long, value_parser = ["srt", "vtt"])]
        format: Option<String>,
        #[arg(long, default_value_t = 0.0)]
        offset: f64,
        #[arg(long, default_value_t = 1.0)]
        scale: f64,
        #[arg(long, default_value_t = 8)]
        max_words: usize,
        #[arg(long, default_value_t = 4.0)]
        max_duration: f64,
        #[arg(long, default_value_t = 80)]
        max_chars: usize,
        #[arg(long)]
        overwrite: bool,
    },
    Mux {
        media: PathBuf,
        #[arg(long)]
        edit: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        overwrite: bool,
    },
}
#[derive(Args)]
struct OutputOptions {
    /// Also save the emitted JSON to this file.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Explicitly permit replacing an existing output file.
    #[arg(long)]
    overwrite: bool,
}
#[derive(Args)]
struct ApiOptions {
    /// Shared cache root (default XDG cache or ~/.cache/skywrite).
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    /// Disable reading and writing response cache.
    #[arg(long)]
    no_cache: bool,
}
#[derive(Args)]
struct AnalyzeArgs {
    input: PathBuf,
    #[arg(long, default_value_t = 8)]
    frames: usize,
    /// Override the content-addressed frame cache root.
    #[arg(long)]
    frames_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 0.0)]
    start: f64,
    #[arg(long)]
    duration: Option<f64>,
    /// Only sample local frames and metadata; do not call OpenRouter.
    #[arg(long, conflicts_with = "model")]
    local: bool,
    /// OpenRouter model ID; alternatively set OPENROUTER_VISION_MODEL.
    #[arg(long)]
    model: Option<String>,
    /// Existing timestamped transcript of this same media file.
    #[arg(long)]
    transcript: Option<PathBuf>,
    /// Editorial brief to guide observations and candidate selects.
    #[arg(long)]
    prompt: Option<String>,
    #[command(flatten)]
    api: ApiOptions,
    #[command(flatten)]
    output: OutputOptions,
}
#[derive(Args)]
struct TranscribeArgs {
    input: PathBuf,
    #[arg(long)]
    model: Option<String>,
    /// Hint the spoken language (ISO code). Only use it when ALL speech is in this
    /// language: Whisper models write other-language speech translated into the
    /// hint (Spanish "con la mano" became "With my hand" under --language en).
    /// Omit it for bilingual footage.
    #[arg(long)]
    language: Option<String>,
    #[arg(long, default_value_t = 0.0)]
    start: f64,
    #[arg(long)]
    duration: Option<f64>,
    #[arg(long, default_value_t = 60.0)]
    chunk_seconds: f64,
    #[command(flatten)]
    api: ApiOptions,
    #[command(flatten)]
    output: OutputOptions,
}
#[derive(Copy, Clone, ValueEnum)]
enum SyncMethod {
    Auto,
    Transcript,
    Audio,
}
#[derive(Args)]
struct SyncArgs {
    #[arg(long)]
    reference: PathBuf,
    #[arg(long)]
    asset: PathBuf,
    #[arg(long,value_enum,default_value_t=SyncMethod::Auto)]
    method: SyncMethod,
    /// Maximum fractional clock-scale deviation (0.03 = +/-3%).
    #[arg(long, default_value_t = 0.03)]
    max_scale_deviation: f64,
    #[arg(long, default_value_t = 0.15)]
    tolerance_seconds: f64,
    #[arg(long, default_value_t = 8)]
    min_anchors: usize,
    #[arg(long, default_value_t = 10.0)]
    min_span_seconds: f64,
    /// Maximum audio prefix decoded per asset; longer inputs produce a warning.
    #[arg(long, default_value_t = 3600.0)]
    max_audio_seconds: f64,
    #[command(flatten)]
    output: OutputOptions,
}
#[derive(Subcommand)]
enum EditCommand {
    /// Check sources, ranges, layers and frame rounding without rendering.
    Validate { file: PathBuf },
    /// Write an edit's caption track (output-timeline cues) as SRT or VTT for an NLE.
    Captions {
        file: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Caption track index in the edit.
        #[arg(long, default_value_t = 0)]
        track: usize,
        #[arg(long, value_parser = ["srt", "vtt"])]
        format: Option<String>,
        #[arg(long)]
        overwrite: bool,
    },
    /// Read a Premiere/Final Cut Pro 7 XML timeline (or, experimentally, a .prproj) into a version 2 edit.
    Import {
        file: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        overwrite: bool,
    },
    /// Convert a version 1 edit (sequential clips) to the layered version 2 format.
    Migrate {
        file: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        overwrite: bool,
    },
    /// Write an NLE interchange (FCPXML, Premiere XML), FFmpeg script, or composition project.
    Export {
        file: PathBuf,
        /// premiere writes Final Cut Pro 7 XML (xmeml v5), which Premiere Pro imports.
        #[arg(long,value_parser=["fcpxml","premiere","ffmpeg","hyperframes","remotion"])]
        to: String,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        overwrite: bool,
    },
    /// Render an H.264/AAC MP4 with FFmpeg, applying layers, fades, gain, LUTs and limiter.
    Render {
        file: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        overwrite: bool,
        /// Keep only the sound of tracks with these roles (for example `--roles dialogue`
        /// for a dialogue-only check render). Every audible track must declare a role.
        #[arg(long, value_enum, value_delimiter = ',')]
        roles: Vec<skywrite::timeline::Role>,
    },
    /// Draft an edit from existing analyses and an editorial brief, then validate it.
    Suggest {
        #[arg(long,required=true,num_args=1..)]
        analysis: Vec<PathBuf>,
        /// Attach existing word timestamps without another transcription request.
        #[arg(long)]
        transcript: Vec<PathBuf>,
        #[arg(long)]
        brief: String,
        #[arg(long)]
        model: Option<String>,
        #[command(flatten)]
        api: ApiOptions,
        #[command(flatten)]
        output: OutputOptions,
    },
}

#[derive(Subcommand)]
enum ImageCommand {
    Render {
        #[arg(long)]
        file: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long, default_value_t = 1280)]
        width: u32,
        #[arg(long, default_value_t = 720)]
        height: u32,
        #[arg(long,value_parser=["png","webp","jpeg"])]
        format: Option<String>,
        #[arg(long)]
        quality: Option<u8>,
        #[arg(long)]
        font: Vec<String>,
        /// Explicit Google Fonts family/variant (repeatable); downloads are cached.
        #[arg(long)]
        google_font: Vec<String>,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        /// Use already cached Google Fonts without network requests.
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        overwrite: bool,
    },
}

#[derive(Subcommand)]
enum ModelCommand {
    /// List capabilities and prices from free cached OpenRouter catalogs.
    List {
        #[arg(long,value_parser=skywrite::models::task_parser())]
        task: Option<String>,
        #[arg(long)]
        input: Option<String>,
        #[arg(long)]
        output: Option<String>,
        #[arg(long)]
        search: Option<String>,
        #[arg(long, conflicts_with = "offline")]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Read saved defaults and task environment variable names.
    Defaults,
    /// Save an explicit model ID; does not call or pay for the model.
    SetDefault {
        #[arg(value_parser=skywrite::models::task_parser())]
        task: String,
        model: String,
    },
    /// Remove a saved task model default without making a paid request.
    UnsetDefault {
        #[arg(value_parser=skywrite::models::task_parser())]
        task: String,
    },
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return;
        }
        Err(error) => {
            let _ = writeln!(
                std::io::stderr(),
                "{}",
                json!({"error":{"code":"invalid_arguments","message":error.to_string()}})
            );
            std::process::exit(2);
        }
    };
    match execute(cli.command) {
        Ok(value) => {
            let mut bytes = serde_json::to_vec_pretty(&value).expect("JSON value serializes");
            bytes.push(b'\n');
            if let Err(error) = std::io::stdout().lock().write_all(&bytes)
                && error.kind() != std::io::ErrorKind::BrokenPipe
            {
                let _ = writeln!(
                    std::io::stderr(),
                    "{}",
                    json!({"error":{"code":"output_failed","message":error.to_string()}})
                );
                std::process::exit(1);
            }
        }
        Err(error) => {
            let report = error
                .downcast_ref::<skywrite::verify::CheckFailed>()
                .map(|e| &e.report)
                .or_else(|| {
                    error
                        .downcast_ref::<skywrite::public_access::CheckFailed>()
                        .map(|e| &e.report)
                });
            if let Some(report) = report {
                let _ = writeln!(std::io::stdout(), "{}", report);
                std::process::exit(1);
            }
            let _ = writeln!(
                std::io::stderr(),
                "{}",
                json!({"error":{"code":"operation_failed","message":format!("{error:#}")}})
            );
            std::process::exit(1);
        }
    }
}
fn api(options: &ApiOptions) -> Result<skywrite::openrouter::OpenRouter> {
    let cache = skywrite::cache::cache_root(options.cache_dir.as_deref())?.join("responses");
    skywrite::openrouter::OpenRouter::new(if options.no_cache { None } else { Some(&cache) })
}
fn finish(value: Value, output: &OutputOptions) -> Result<Value> {
    if let Some(path) = &output.output {
        skywrite::io::write_json(path, &value, output.overwrite)?;
    }
    Ok(value)
}
fn execute(command: Command) -> Result<Value> {
    match command {
        Command::Storage { command } => skywrite::storage::execute(command),
        Command::Outputs { command } => skywrite::outputs::execute(command),
        Command::Defaults(args) => skywrite::config_cli::execute_defaults(args),
        Command::Init(args) => skywrite::config_cli::initialize(args),
        Command::Generate { command } => skywrite::generation::execute_generate(command),
        Command::Jobs { command } => skywrite::generation::execute_jobs(command),
        Command::Project { command } => skywrite::project::execute(command),
        Command::Audio { command } => skywrite::audio::execute(command),
        Command::Captions { command } => match command {
            CaptionsCommand::Convert {
                transcript,
                output,
                format,
                offset,
                scale,
                max_words,
                max_duration,
                max_chars,
                overwrite,
            } => {
                let format = format
                    .as_deref()
                    .or_else(|| output.extension().and_then(|s| s.to_str()))
                    .context("caption output requires .srt/.vtt extension or --format")?;
                skywrite::captions::convert(
                    &transcript,
                    &output,
                    format,
                    &skywrite::captions::ConvertOptions {
                        offset,
                        scale,
                        max_words,
                        max_duration,
                        max_chars,
                    },
                    overwrite,
                )
            }
            CaptionsCommand::Mux {
                media,
                edit,
                output,
                overwrite,
            } => skywrite::captions::mux(
                &media,
                &skywrite::timeline::load_and_validate(&edit)?,
                &output,
                overwrite,
            ),
        },
        Command::Upscale { command } => skywrite::generation::execute_upscale(command),

        Command::Doctor => {
            let mut tools = serde_json::Map::new();
            for (tool, flag) in [
                ("ffmpeg", "-version"),
                ("ffprobe", "-version"),
                ("hyperframes", "--version"),
                ("node", "--version"),
            ] {
                let result = skywrite::process::run(
                    tool,
                    &skywrite::process::args(&[flag]),
                    Duration::from_secs(15),
                    256 * 1024,
                );
                tools.insert(tool.to_owned(),match result {Ok(out)=>json!({"available":true,"version":String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("")}),Err(error)=>json!({"available":false,"reason":error.to_string()})});
            }
            Ok(
                json!({"version":env!("CARGO_PKG_VERSION"),"tools":tools,"openrouter_key_present":std::env::var_os("OPENROUTER_API_KEY").is_some(),"network_checked":false,"required_tools":["ffmpeg","ffprobe"],"optional_tools":["hyperframes","node (Remotion)"],"native_image_renderer":{"name":"takumi","version":"2.19.0","available":true}}),
            )
        }
        Command::Verify(args) => skywrite::verify::execute(args),
        Command::Review { command } => skywrite::review::execute(command),
        Command::Describe { command } => skywrite::describe::execute(command),
        Command::Inspect {
            inputs,
            exact_audio,
            cache_dir,
            output,
        } => {
            preflight_output(&output)?;
            protect_inputs(&output, &inputs)?;
            let media = inputs
                .iter()
                .map(|p| -> Result<Value> {
                    let mut info = skywrite::media::probe(p)?;
                    let timing = if exact_audio && info.has_audio() {
                        Some(skywrite::media::exact_audio_timing(
                            &info,
                            cache_dir.as_deref(),
                        )?)
                    } else {
                        None
                    };
                    if !info.has_video()
                        && let Some(timing) = &timing
                    {
                        info.duration = timing.presentation_end_seconds;
                    }
                    let basis = if timing.is_some() && !info.has_video() {
                        "decoded_audio_presentation"
                    } else {
                        "metadata_estimate"
                    };
                    let color = if info.has_video() {
                        Some(skywrite::color::report(&info.path, &info.format_name)?)
                    } else {
                        None
                    };
                    let mut value = serde_json::to_value(info)?;
                    value["duration_basis"] = json!(basis);
                    value["color"] = json!(color);
                    value["audio_timing"] = serde_json::to_value(timing)?;
                    Ok(value)
                })
                .collect::<Result<Vec<_>>>()?;
            finish(json!({"version":1,"media":media}), &output)
        }
        Command::Transcribe(args) => {
            preflight_output(&args.output)?;
            protect_inputs(&args.output, std::slice::from_ref(&args.input))?;
            let info = skywrite::media::probe_for_timing(&args.input)?;
            let model = skywrite::models::resolve_model("transcription", args.model.as_deref())?;
            let result = skywrite::transcript::transcribe(
                &info,
                &api(&args.api)?,
                &skywrite::transcript::TranscribeOptions {
                    model: &model,
                    language: args.language.as_deref(),
                    start: args.start,
                    duration: args.duration,
                    chunk_seconds: args.chunk_seconds,
                },
            )?;
            finish(serde_json::to_value(result)?, &args.output)
        }
        Command::Analyze(args) => {
            preflight_output(&args.output)?;
            let inputs: Vec<_> = std::iter::once(args.input.clone())
                .chain(args.transcript.iter().cloned())
                .collect();
            protect_inputs(&args.output, &inputs)?;
            let info = skywrite::media::probe_for_timing(&args.input)?;
            let model = if args.local {
                None
            } else {
                Some(skywrite::models::resolve_model(
                    "analysis",
                    args.model.as_deref(),
                )?)
            };
            let frames_dir = match args.frames_dir {
                Some(path) => path,
                None => skywrite::cache::cache_root(args.api.cache_dir.as_deref())?.join("assets"),
            };
            let client = if args.local {
                None
            } else {
                Some(api(&args.api)?)
            };
            let result = skywrite::analysis::analyze(
                info,
                client.as_ref(),
                &skywrite::analysis::AnalyzeOptions {
                    frames: args.frames,
                    frames_dir: &frames_dir,
                    start: args.start,
                    duration: args.duration,
                    model: if args.local { None } else { model.as_deref() },
                    transcript: args.transcript.as_deref(),
                    prompt: args.prompt.as_deref(),
                },
            )?;
            // Cached sample files are inputs to the visual request as well.
            protect_inputs(
                &args.output,
                &result
                    .frames
                    .iter()
                    .map(|frame| frame.path.clone())
                    .collect::<Vec<_>>(),
            )?;
            finish(serde_json::to_value(result)?, &args.output)
        }
        Command::Sync(args) => {
            preflight_output(&args.output)?;
            protect_inputs(&args.output, &[args.reference.clone(), args.asset.clone()])?;
            let options = skywrite::sync::SyncOptions {
                max_scale_deviation: args.max_scale_deviation,
                tolerance_seconds: args.tolerance_seconds,
                min_anchors: args.min_anchors,
                min_span_seconds: args.min_span_seconds,
                max_audio_seconds: args.max_audio_seconds,
            };
            let transcripts = match args.method {
                SyncMethod::Transcript => true,
                SyncMethod::Audio => false,
                SyncMethod::Auto => {
                    args.reference.extension().is_some_and(|e| e == "json")
                        && args.asset.extension().is_some_and(|e| e == "json")
                }
            };
            let result = if transcripts {
                let reference = skywrite::transcript::Transcript::load(&args.reference)?;
                let asset = skywrite::transcript::Transcript::load(&args.asset)?;
                protect_inputs(
                    &args.output,
                    &[
                        relative_input(&args.reference, &reference.source),
                        relative_input(&args.asset, &asset.source),
                    ],
                )?;
                skywrite::sync::align_transcripts(&reference, &asset, &options)?
            } else {
                skywrite::sync::align_audio(&args.reference, &args.asset, &options)?
            };
            finish(serde_json::to_value(result)?, &args.output)
        }
        Command::Edit { command } => match command {
            EditCommand::Validate { file } => {
                Ok(skywrite::timeline::load_and_validate(&file)?.summary())
            }
            EditCommand::Captions {
                file,
                output,
                track,
                format,
                overwrite,
            } => {
                skywrite::io::protect_inputs(&output, std::slice::from_ref(&file))?;
                let edit = skywrite::timeline::load_and_validate(&file)?;
                skywrite::export::write_caption_track(
                    &edit,
                    track,
                    &output,
                    format.as_deref(),
                    overwrite,
                )
            }
            EditCommand::Import {
                file,
                output,
                overwrite,
            } => {
                skywrite::io::protect_inputs(&output, std::slice::from_ref(&file))?;
                let (edit, notes) = skywrite::import::import(&file)?;
                // Check structure before writing; media may live on another machine.
                skywrite::timeline::parse(edit.clone())
                    .context("imported timeline is not a valid edit")?;
                skywrite::io::write_json(&output, &edit, overwrite)?;
                let validation = match skywrite::timeline::load_and_validate(&output) {
                    Ok(validated) => validated.summary(),
                    Err(error) => {
                        json!({"valid":false,"error":format!("{error:#}"),"hint":"Media paths are absolute from the exporting machine; fix them or relink, then run skywrite edit validate."})
                    }
                };
                Ok(json!({"input":file,"output":output,"notes":notes,"validation":validation}))
            }
            EditCommand::Migrate {
                file,
                output,
                overwrite,
            } => {
                skywrite::io::protect_inputs(&output, std::slice::from_ref(&file))?;
                let old: Value = skywrite::io::read_json(&file)?;
                let new = skywrite::timeline::migrate_v1(&old)?;
                skywrite::io::write_json(&output, &new, overwrite)?;
                Ok(
                    json!({"input":file,"output":output,"version":skywrite::timeline::EDIT_VERSION,
                    "next_command":format!("skywrite edit validate {}", output.display()),
                    "note":"Primary clips are now on video track V1 at explicit start times; overlay audio is packed onto audio tracks. Relative source paths still resolve against the new file's directory."}),
                )
            }
            EditCommand::Export {
                file,
                to,
                output,
                overwrite,
            } => {
                skywrite::io::protect_inputs(&output, std::slice::from_ref(&file))?;
                skywrite::export::export(
                    &skywrite::timeline::load_and_validate(&file)?,
                    &to,
                    &output,
                    overwrite,
                )
            }
            EditCommand::Render {
                file,
                output,
                overwrite,
                roles,
            } => {
                skywrite::io::protect_inputs(&output, std::slice::from_ref(&file))?;
                let mut edit = skywrite::timeline::load_and_validate(&file)?;
                if !roles.is_empty() {
                    edit = edit.with_roles(&roles)?;
                }
                let mut result = skywrite::export::render(&edit, &output, overwrite)?;
                if !roles.is_empty() {
                    result["roles"] = json!(roles);
                }
                Ok(result)
            }
            EditCommand::Suggest {
                analysis,
                transcript,
                brief,
                model,
                api: options,
                output,
            } => suggest(
                &analysis,
                &transcript,
                &brief,
                model.as_deref(),
                &options,
                &output,
            ),
        },
        Command::Models { command } => match command {
            ModelCommand::List {
                task,
                input,
                output,
                search,
                refresh,
                offline,
                cache_dir,
            } => skywrite::models::list(&skywrite::models::ListOptions {
                task: task.as_deref(),
                input: input.as_deref(),
                output: output.as_deref(),
                search: search.as_deref(),
                refresh,
                offline,
                cache_dir: cache_dir.as_deref(),
            }),
            ModelCommand::Defaults => Ok(
                json!({"defaults":skywrite::workspace::model_defaults()?,"task_environment":skywrite::models::TASKS.iter().map(|task|Ok((task.to_string(),skywrite::models::task_environment(task)?))).collect::<Result<std::collections::BTreeMap<_,_>>>()?,"precedence":["--model","task environment (or legacy OPENROUTER_VISION_MODEL for analysis/editing)","saved task default","transcription fallback only: elevenlabs/scribe-v2"]}),
            ),
            ModelCommand::SetDefault { task, model } => {
                skywrite::workspace::set_default_model(&task, &model)
            }
            ModelCommand::UnsetDefault { task } => skywrite::workspace::unset_default_model(&task),
        },
        Command::Styles { command } => {
            skywrite::workspace::execute_content(skywrite::workspace::ContentKind::Styles, command)
        }
        Command::Skills { command } => {
            skywrite::workspace::execute_content(skywrite::workspace::ContentKind::Skills, command)
        }
        Command::Nit { description } => skywrite::workspace::record_nit(&description),
        Command::Schema => Ok(schema()),
        Command::Image { command } => match command {
            ImageCommand::Render {
                file,
                output,
                width,
                height,
                format,
                quality,
                font,
                google_font,
                cache_dir,
                offline,
                overwrite,
            } => skywrite::image::render(&skywrite::image::RenderImage {
                file: &file,
                output: &output,
                width,
                height,
                format: format.as_deref(),
                quality,
                fonts: &font,
                google_fonts: &google_font,
                cache_dir: cache_dir.as_deref(),
                offline,
                overwrite,
            }),
        },
    }
}
fn preflight_output(output: &OutputOptions) -> Result<()> {
    if output.output.as_ref().is_some_and(|p| p.exists()) && !output.overwrite {
        bail!("output exists; use --overwrite to replace it");
    }
    Ok(())
}
fn suggest(
    analyses: &[PathBuf],
    transcripts: &[PathBuf],
    brief: &str,
    model: Option<&str>,
    options: &ApiOptions,
    output: &OutputOptions,
) -> Result<Value> {
    preflight_output(output)?;
    let input_paths: Vec<_> = analyses.iter().chain(transcripts).cloned().collect();
    protect_inputs(output, &input_paths)?;
    if analyses.len() > 32 || transcripts.len() > 32 || brief.len() > 16 * 1024 {
        bail!("suggest supports at most 32 analyses, 32 transcripts and a 16-KiB brief");
    }
    let admitted = skywrite::io::read_files_bounded(&input_paths, 8 * 1024 * 1024)
        .context("suggestion input admission (eight MiB combined)")?;
    let mut reports = Vec::new();
    let mut allowed = std::collections::HashSet::new();
    let mut dependencies = Vec::new();
    let allowed_captions = transcripts
        .iter()
        .map(|p| skywrite::media::canonical_file(p))
        .collect::<Result<std::collections::HashSet<_>>>()?;
    for (file, bytes) in analyses.iter().zip(&admitted) {
        let report: skywrite::analysis::Analysis = serde_json::from_slice(bytes)
            .with_context(|| format!("invalid analysis JSON in {}", file.display()))?;
        allowed.insert(skywrite::media::canonical_file(&report.media.path)?);
        dependencies.extend(
            report
                .frames
                .iter()
                .map(|frame| relative_input(file, &frame.path)),
        );
        if let Some(transcript) = &report.transcript {
            dependencies.push(relative_input(file, &transcript.source));
        }
        reports.push(report);
    }
    let mut attached = Vec::new();
    for (file, bytes) in transcripts.iter().zip(admitted.iter().skip(analyses.len())) {
        let mut transcript: skywrite::transcript::Transcript = serde_json::from_slice(bytes)
            .with_context(|| format!("invalid transcript JSON in {}", file.display()))?;
        transcript.source = relative_input(file, &transcript.source);
        dependencies.push(transcript.source.clone());
        attached.push(transcript);
    }
    protect_inputs(output, &allowed.iter().cloned().collect::<Vec<_>>())?;
    protect_inputs(output, &dependencies)?;
    skywrite::analysis::attach_transcripts(&mut reports, &attached)?;
    let model = skywrite::models::resolve_model("editing", model)?;
    let encoded = serde_json::to_vec(&skywrite::analysis::suggestion_context(&reports)?)?;
    if encoded.len() > 8 * 1024 * 1024 {
        bail!("combined analysis reports exceed eight MiB; select fewer sources/ranges");
    }
    let prompt =
        skywrite::analysis::suggest_prompt(brief, &schema()["edit"], &String::from_utf8(encoded)?);
    let response=api(options)?.request("chat/completions",&json!({"model":model,"messages":[{"role":"user","content":prompt}],"response_format":{"type":"json_object"},"max_tokens":4000,"temperature":0.2}))?;
    let draft = skywrite::openrouter::assistant_json(&response.body)?;
    let mut referenced = Vec::new();
    for kind in ["video", "audio"] {
        for track in draft[kind].as_array().into_iter().flatten() {
            for clip in track["clips"].as_array().into_iter().flatten() {
                referenced.push((false, clip["source"].clone()));
            }
        }
    }
    for caption in draft["captions"].as_array().into_iter().flatten() {
        referenced.push((true, caption["source"].clone()));
    }
    ensure!(
        draft.get("sources").is_none(),
        "suggested edits must not declare source color settings or LUTs; add them yourself"
    );
    for (caption, source) in referenced {
        let Some(source) = source.as_str() else {
            bail!("suggested edit has a clip without a string source");
        };
        let path = Path::new(source);
        if !path.is_absolute()
            || !(if caption { &allowed_captions } else { &allowed })
                .contains(&skywrite::media::canonical_file(path)?)
        {
            bail!("suggested edit references a source outside the provided analyses/transcripts");
        }
    }
    let edit =
        skywrite::timeline::parse(draft).context("model produced an invalid edit structure")?;
    let base = std::env::current_dir()?;
    let validated = skywrite::timeline::validate(&edit, &base)
        .context("model edit failed validation; no output written")?;
    eprintln!(
        "{}",
        json!({"suggest_usage":response.body.get("usage"),"cached":response.cached,"validation":validated.summary(),"dialogue_cut_warnings":skywrite::analysis::dialogue_cut_warnings(&validated,&reports)?})
    );
    finish(serde_json::to_value(edit)?, output)
}
fn schema() -> Value {
    let number = |min: f64, max: f64, default: f64, description: &str| json!({"type":"number","minimum":min,"maximum":max,"default":default,"description":description});
    let keyframes = json!({"type":"array","maxItems":256,"description":"Linear gain points; time is OUTPUT seconds from the clip start, strictly increasing. Gain interpolates linearly and holds outside the points. Effective gain = volume * envelope * fades.",
        "items":{"type":"object","additionalProperties":false,"required":["time","volume"],"properties":{"time":{"type":"number","minimum":0},"volume":{"type":"number","minimum":0,"maximum":4}}}});
    let mix = json!({"type":"object","additionalProperties":false,"properties":{
        "volume":number(0.0,4.0,1.0,"Linear gain; 0 mutes"),"volume_keyframes":keyframes,
        "fade_in":number(0.0,86400.0,0.0,"Linear fade-in, output seconds"),"fade_out":number(0.0,86400.0,0.0,"Linear fade-out, output seconds")}});
    let placement = json!({
        "start":{"type":"number","minimum":0,"description":"Timeline position in OUTPUT seconds, rounded to the frame grid"},
        "source":{"type":"string","description":"Local media path relative to the edit file"},
        "source_in":{"type":"number","minimum":0,"default":0,"description":"Source seconds from the first presented sample"},
        "duration":{"type":"number","exclusiveMinimum":0,"maximum":86400,"description":"OUTPUT duration in seconds, on the frame grid"},
        "speed":{"type":"number","minimum":0.125,"maximum":8,"default":1,"description":"Playback multiplier: consumed source seconds = duration * speed"}});
    let mut video_clip = placement.clone();
    video_clip["opacity"] = number(
        0.0,
        1.0,
        1.0,
        "Picture opacity over lower tracks (black at the bottom)",
    );
    video_clip["fade_in"] = number(
        0.0,
        86400.0,
        0.0,
        "Picture fade from transparent, output seconds",
    );
    video_clip["fade_out"] = number(
        0.0,
        86400.0,
        0.0,
        "Picture fade to transparent, output seconds",
    );
    video_clip["audio"] = mix.clone();
    let mut audio_clip = placement;
    for (key, value) in mix["properties"].as_object().expect("object") {
        audio_clip[key] = value.clone();
    }
    let track = |clip: Value| {
        json!({"type":"object","additionalProperties":false,"properties":{
        "name":{"type":"string","maxLength":64},
        "role":{"enum":["dialogue","music","effects"],"description":"What this track's sound is (for a video track, its linked sound). Used by edit render --roles, FCPXML audio roles and review lint"},
        "clips":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["start","source","duration"],"properties":clip}}}})
    };
    let color = json!({"type":"object","additionalProperties":false,"description":"Operator-confirmed capture encoding; overrides container metadata in reports, does not transform pixels",
        "properties":{"transfer":{"type":"string","examples":["canon-log2","canon-log3","s-log3","bt709"]},"primaries":{"type":"string","examples":["cinema-gamut","bt2020","bt709"]},"matrix":{"type":"string"},"note":{"type":"string"}}});
    let edit = json!({"$schema":"https://json-schema.org/draft/2020-12/schema","title":"Skywrite Edit v2","type":"object","additionalProperties":false,"required":["version"],"properties":{
        "version":{"const":2},"name":{"type":"string","default":"Skywrite edit","minLength":1,"maxLength":512,"description":"Nonblank; max 512 UTF-8 bytes; XML controls prohibited"},"width":{"type":"integer","minimum":2,"maximum":8192,"multipleOf":2,"default":1920},"height":{"type":"integer","minimum":2,"maximum":8192,"multipleOf":2,"default":1080},
        "fps":{"type":"object","required":["num","den"],"additionalProperties":false,"properties":{"num":{"type":"integer","minimum":1,"maximum":1000000},"den":{"type":"integer","minimum":1,"maximum":1000000}}},
        "duration":{"type":"number","exclusiveMinimum":0,"maximum":86400,"description":"Optional timeline length; defaults to the latest clip end"},
        "sources":{"type":"array","maxItems":256,"items":{"type":"object","additionalProperties":false,"required":["path"],"properties":{"path":{"type":"string"},"lut":{"type":"string","description":".cube LUT applied to this source in rendered MP4 output; NLE exports warn instead"},"color":color}}},
        "video":{"type":"array","maxItems":16,"description":"Video tracks; index 0 is the bottom layer. Clips on one track must not overlap.","items":track(video_clip)},
        "audio":{"type":"array","maxItems":32,"description":"Audio-only tracks, mixed with linked sound from video clips","items":track(audio_clip)},
        "master":{"type":"object","additionalProperties":false,"properties":{"limiter_db":{"type":"number","minimum":-24,"maximum":0,"description":"Optional output limiter ceiling in dBFS. Without it, the mix is summed at unity gain and may clip."}}},
        "captions":{"type":"array","description":"Caption tracks: {\"from_transcripts\":[\"words.json\"],\"language\":\"eng\"} maps source words through the edit (recommended), or {\"source\":\"cues.srt\"} for cues already on the output timeline. See docs/edit-format.md"},"chapters":{"type":"array","description":"See docs/edit-format.md"}
    }});
    json!({"version":2,"edit":edit,"edit_example":{"version":2,"name":"Interview","width":1280,"height":720,"fps":{"num":30,"den":1},
        "video":[
            {"name":"V1","clips":[{"start":0,"source":"camera.mp4","source_in":12.4,"duration":9,"audio":{"volume":0}},{"start":9,"source":"title.png","duration":3,"fade_out":1}]},
            {"name":"V2 B-roll","clips":[{"start":4,"source":"broll.mp4","source_in":30,"duration":3,"fade_in":0.5,"audio":{"volume":0}}]}],
        "audio":[
            {"name":"Dialogue","clips":[{"start":0,"source":"recorder.wav","source_in":20.1,"duration":9,"fade_out":0.25}]},
            {"name":"Music","clips":[{"start":0,"source":"bed.mp3","duration":12,"volume":0.5,"volume_keyframes":[{"time":0,"volume":1},{"time":1,"volume":0.3},{"time":8.5,"volume":0.3},{"time":9.5,"volume":1}],"fade_out":2}]}],
        "master":{"limiter_db":-1}},
        "transcript_example":{"version":1,"source":"/absolute/path/camera.mp4","duration":20,"text":"Hello world","words":[{"text":"Hello","start":2.1,"end":2.4},{"text":"world","start":2.5,"end":2.8}]},
        "sync_mapping":{"formula":"reference_time = offset_seconds + scale * asset_time","playback_rate":"1 / scale","timeline_speed":"Use playback_rate when placing asset on reference timeline; trim overlap explicitly."},
        "notes":["JSON Schema is discovery guidance; edit validate also checks frame alignment, overlaps, source media bounds, LUT files and actual sources.",
            "Every clip has an explicit timeline start. Video track 0 is the bottom layer; empty regions are black and silent. Upper-track pictures composite over lower ones, and letterboxing is transparent.",
            "Version 1 edits (top-level clips) are rejected; run skywrite edit migrate OLD.json --output NEW.json.",
            "The rendered MP4 applies everything here. FCPXML/Premiere XML carry layers, speed, opacity, fades and gain; they warn about LUTs and the master limiter, which must be applied in the NLE.",
            "Transitions, titles, transforms and grading beyond a LUT belong in an NLE or composition project."]})
}

fn protect_inputs(output: &OutputOptions, inputs: &[PathBuf]) -> Result<()> {
    if let Some(path) = &output.output {
        skywrite::io::protect_inputs(path, inputs)?;
    }
    Ok(())
}
fn relative_input(document: &Path, input: &Path) -> PathBuf {
    if input.is_absolute() {
        input.to_owned()
    } else {
        document.parent().unwrap_or(Path::new(".")).join(input)
    }
}
