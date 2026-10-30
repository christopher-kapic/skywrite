use anyhow::{Context, Result, bail};
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
    after_help = "Run skywrite init for offline starter preferences and skywrite defaults image to inspect a task. All successful commands emit JSON to stdout. Diagnostics go to stderr. Agents: save ALL frustrations, bugs, and missing features with skywrite nit <description>. Run skywrite schema for edit/transcript examples. Compose tools: render HTML graphics as image clips in FCPXML/FFmpeg/Hyperframes/Remotion edits; generate character images and reference frames before video generation."
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
        #[command(flatten)]
        output: OutputOptions,
    },
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
    /// Save ALL agent frustrations in ~/.skywrite/nits (never uploads).
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
    /// Print agent-facing edit and transcript schemas/examples.
    Schema,
    /// Render still assets such as title cards using embedded upstream Takumi.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
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
    Validate {
        file: PathBuf,
    },
    Export {
        file: PathBuf,
        #[arg(long,value_parser=["fcpxml","ffmpeg","hyperframes","remotion"])]
        to: String,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        overwrite: bool,
    },
    Render {
        file: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        overwrite: bool,
    },
    /// Draft an edit from existing analyses and an editorial brief, then validate it.
    Suggest {
        #[arg(long,required=true,num_args=1..)]
        analysis: Vec<PathBuf>,
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
        Command::Inspect { inputs, output } => {
            protect_inputs(&output, &inputs)?;
            let media = inputs
                .iter()
                .map(|p| skywrite::media::probe(p))
                .collect::<Result<Vec<_>>>()?;
            finish(json!({"version":1,"media":media}), &output)
        }
        Command::Transcribe(args) => {
            preflight_output(&args.output)?;
            protect_inputs(&args.output, std::slice::from_ref(&args.input))?;
            let info = skywrite::media::probe(&args.input)?;
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
            let info = skywrite::media::probe(&args.input)?;
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
            } => {
                skywrite::io::protect_inputs(&output, std::slice::from_ref(&file))?;
                skywrite::export::render(
                    &skywrite::timeline::load_and_validate(&file)?,
                    &output,
                    overwrite,
                )
            }
            EditCommand::Suggest {
                analysis,
                brief,
                model,
                api: options,
                output,
            } => suggest(&analysis, &brief, model.as_deref(), &options, &output),
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
                json!({"defaults":skywrite::workspace::model_defaults()?,"task_environment":skywrite::models::TASKS.iter().map(|task|Ok((task.to_string(),skywrite::models::task_environment(task)?))).collect::<Result<std::collections::BTreeMap<_,_>>>()?,"precedence":["--model","task environment (or legacy OPENROUTER_VISION_MODEL for analysis/editing)","saved task default","transcription fallback only: openai/whisper-large-v3"]}),
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
    brief: &str,
    model: Option<&str>,
    options: &ApiOptions,
    output: &OutputOptions,
) -> Result<Value> {
    preflight_output(output)?;
    protect_inputs(output, analyses)?;
    if analyses.len() > 32 || brief.len() > 16 * 1024 {
        bail!("suggest supports at most 32 analyses and a 16-KiB brief");
    }
    let mut reports = Vec::new();
    let mut allowed = std::collections::HashSet::new();
    let mut dependencies = Vec::new();
    for file in analyses {
        let report: skywrite::analysis::Analysis = skywrite::io::read_json(file)?;
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
    protect_inputs(output, &allowed.iter().cloned().collect::<Vec<_>>())?;
    protect_inputs(output, &dependencies)?;
    let model = skywrite::models::resolve_model("editing", model)?;
    let encoded = serde_json::to_vec(&reports)?;
    if encoded.len() > 8 * 1024 * 1024 {
        bail!("combined analysis reports exceed eight MiB; select fewer sources/ranges");
    }
    let prompt = format!(
        "Create a coherent, well-paced assembly edit following this brief: {brief}\nUse only the explicitly listed analyzed source paths. Respect source durations; do not invent dialogue or unsampled actions. Prefer complete thoughts and purposeful cuts. You can include black gaps with source:null and separate audio overlays. Return ONLY an edit JSON object conforming to this schema: {}\nAll source paths must be ABSOLUTE paths copied from the analyses. Output duration is timeline seconds; source consumption = duration*speed, so normal speed=1. Quantize durations and offsets to the requested frame grid. No titles/effects/unsupported fields. The default target is 1280x720 at 30/1 fps unless brief requests otherwise. Analyses: {}",
        schema()["edit"],
        String::from_utf8(encoded)?
    );
    let response=api(options)?.request("chat/completions",&json!({"model":model,"messages":[{"role":"user","content":prompt}],"response_format":{"type":"json_object"},"max_tokens":4000,"temperature":0.2}))?;
    let draft = skywrite::openrouter::assistant_json(&response.body)?;
    for list in ["clips", "audio"] {
        if let Some(clips) = draft[list].as_array() {
            for clip in clips {
                if let Some(source) = clip["source"].as_str() {
                    let path = Path::new(source);
                    if !path.is_absolute()
                        || !allowed.contains(&skywrite::media::canonical_file(path)?)
                    {
                        bail!("suggested edit references a source outside the provided analyses");
                    }
                }
            }
        }
    }
    let edit: skywrite::timeline::Edit =
        serde_json::from_value(draft).context("model produced an invalid edit structure")?;
    let base = std::env::current_dir()?;
    let validated = skywrite::timeline::validate(&edit, &base)
        .context("model edit failed validation; no output written")?;
    eprintln!(
        "{}",
        json!({"suggest_usage":response.body.get("usage"),"cached":response.cached,"validation":validated.summary()})
    );
    finish(serde_json::to_value(edit)?, output)
}
fn schema() -> Value {
    let clip = json!({"type":"object","additionalProperties":false,"required":["duration"],"properties":{
        "source":{"type":["string","null"],"description":"Local media path relative to edit file; null creates black/silent gap"},
        "source_in":{"type":"number","minimum":0,"default":0},
        "duration":{"type":"number","exclusiveMinimum":0,"maximum":86400,"description":"OUTPUT duration in seconds, on sequence frame grid"},
        "speed":{"type":"number","minimum":0.125,"maximum":8,"default":1,"description":"Playback multiplier: consumed source seconds = duration * speed"},
        "volume":{"type":"number","minimum":0,"maximum":4,"default":1}
    }});
    let audio = json!({"type":"object","additionalProperties":false,"required":["source","duration"],"properties":{
        "source":{"type":"string"},"source_in":{"type":"number","minimum":0,"default":0},
        "offset":{"type":"number","minimum":0,"default":0,"description":"Position on output timeline, in seconds"},
        "duration":{"type":"number","exclusiveMinimum":0,"maximum":86400},"speed":{"type":"number","minimum":0.125,"maximum":8,"default":1},"volume":{"type":"number","minimum":0,"maximum":4,"default":1}
    }});
    let edit = json!({"$schema":"https://json-schema.org/draft/2020-12/schema","title":"Skywrite Edit v1","type":"object","additionalProperties":false,"required":["clips"],"properties":{
        "version":{"const":1},"name":{"type":"string","default":"Skywrite edit","minLength":1,"maxLength":512,"description":"Nonblank; max 512 UTF-8 bytes; XML controls prohibited"},"width":{"type":"integer","minimum":2,"maximum":8192,"multipleOf":2,"default":1920},"height":{"type":"integer","minimum":2,"maximum":8192,"multipleOf":2,"default":1080},
        "fps":{"type":"object","required":["num","den"],"additionalProperties":false,"properties":{"num":{"type":"integer","minimum":1,"maximum":1000000},"den":{"type":"integer","minimum":1,"maximum":1000000}}},
        "clips":{"type":"array","minItems":1,"maxItems":512,"items":clip},"audio":{"type":"array","maxItems":256,"items":audio,"default":[]}
    }});
    json!({"version":1,"edit":edit,"edit_example":{"version":1,"name":"Assembly","width":1280,"height":720,"fps":{"num":30,"den":1},"clips":[{"source":"camera.mp4","source_in":2,"duration":5,"speed":1,"volume":0},{"source":null,"duration":1},{"source":"still.png","duration":3}],"audio":[{"source":"recorder.wav","source_in":12,"offset":0,"duration":9,"speed":1,"volume":1}]},
        "transcript_example":{"version":1,"source":"/absolute/path/camera.mp4","duration":20,"text":"Hello world","words":[{"text":"Hello","start":2.1,"end":2.4},{"text":"world","start":2.5,"end":2.8}]},
        "sync_mapping":{"formula":"reference_time = offset_seconds + scale * asset_time","playback_rate":"1 / scale","timeline_speed":"Use playback_rate when placing asset on reference timeline; trim overlap explicitly."},
        "notes":["JSON Schema is discovery guidance; edit validate also checks frame alignment, source media bounds, supported speed/dimensions and actual files.","Primary clips are sequential. Audio overlays have absolute output offsets. Unsupported effects/transitions must be added in an NLE or optional composition editor.","Frame times use rational fps; quantize output duration and audio offset to frame grid. Source in points use seconds."]})
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
