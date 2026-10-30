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
    after_help = "All successful commands emit JSON to stdout. Diagnostics go to stderr. Run skywrite schema for edit/transcript examples."
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
    /// Print agent-facing edit and transcript schemas/examples.
    Schema,
    /// Render still assets such as title cards through the optional Takumi CLI.
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
    /// Exact request/response cache. Contains media-derived model responses.
    #[arg(long, default_value = ".skywrite/cache")]
    cache_dir: PathBuf,
    /// Disable reading and writing response cache.
    #[arg(long)]
    no_cache: bool,
}
#[derive(Args)]
struct AnalyzeArgs {
    input: PathBuf,
    #[arg(long, default_value_t = 8)]
    frames: usize,
    #[arg(long, default_value = ".skywrite/frames")]
    frames_dir: PathBuf,
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
    #[arg(long, default_value = "openai/whisper-large-v3")]
    model: String,
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
    /// Maximum audio duration decoded per asset; longer inputs are rejected.
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
        #[arg(long)]
        overwrite: bool,
        /// Explicit external adapter; current upstream native rendering is not included.
        #[arg(long,default_value="takumi-cli",value_parser=["takumi-cli"])]
        backend: String,
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
    skywrite::openrouter::OpenRouter::new(if options.no_cache {
        None
    } else {
        Some(&options.cache_dir)
    })
}
fn finish(value: Value, output: &OutputOptions) -> Result<Value> {
    if let Some(path) = &output.output {
        skywrite::io::write_json(path, &value, output.overwrite)?;
    }
    Ok(value)
}
fn execute(command: Command) -> Result<Value> {
    match command {
        Command::Doctor => {
            let mut tools = serde_json::Map::new();
            for (tool, flag) in [
                ("ffmpeg", "-version"),
                ("ffprobe", "-version"),
                ("hyperframes", "--version"),
                ("node", "--version"),
                ("takumi", "--help"),
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
                json!({"version":env!("CARGO_PKG_VERSION"),"tools":tools,"openrouter_key_present":std::env::var_os("OPENROUTER_API_KEY").is_some(),"network_checked":false,"required_tools":["ffmpeg","ffprobe"],"optional_tools":["hyperframes","node (Remotion)","takumi"]}),
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
            let result = skywrite::transcript::transcribe(
                &info,
                &api(&args.api)?,
                &skywrite::transcript::TranscribeOptions {
                    model: &args.model,
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
            protect_inputs(&args.output, std::slice::from_ref(&args.input))?;
            let info = skywrite::media::probe(&args.input)?;
            let model = args
                .model
                .or_else(|| std::env::var("OPENROUTER_VISION_MODEL").ok());
            if !args.local && model.is_none() {
                bail!(
                    "remote analysis requires --model or OPENROUTER_VISION_MODEL; use --local to sample frames without OpenRouter"
                );
            }
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
                    frames_dir: &args.frames_dir,
                    start: args.start,
                    duration: args.duration,
                    model: if args.local { None } else { model.as_deref() },
                    transcript: args.transcript.as_deref(),
                    prompt: args.prompt.as_deref(),
                },
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
                skywrite::sync::align_transcripts(
                    &skywrite::transcript::Transcript::load(&args.reference)?,
                    &skywrite::transcript::Transcript::load(&args.asset)?,
                    &options,
                )?
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
            } => skywrite::export::export(
                &skywrite::timeline::load_and_validate(&file)?,
                &to,
                &output,
                overwrite,
            ),
            EditCommand::Render {
                file,
                output,
                overwrite,
            } => skywrite::export::render(
                &skywrite::timeline::load_and_validate(&file)?,
                &output,
                overwrite,
            ),
            EditCommand::Suggest {
                analysis,
                brief,
                model,
                api: options,
                output,
            } => suggest(&analysis, &brief, model.as_deref(), &options, &output),
        },
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
                overwrite,
                backend: _,
            } => skywrite::image::render(&skywrite::image::RenderImage {
                file: &file,
                output: &output,
                width,
                height,
                format: format.as_deref(),
                quality,
                fonts: &font,
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
    let model = model
        .map(str::to_owned)
        .or_else(|| std::env::var("OPENROUTER_VISION_MODEL").ok())
        .context("edit suggest requires --model or OPENROUTER_VISION_MODEL")?;
    let mut reports = Vec::new();
    let mut allowed = std::collections::HashSet::new();
    for file in analyses {
        let report: skywrite::analysis::Analysis = skywrite::io::read_json(file)?;
        allowed.insert(skywrite::media::canonical_file(&report.media.path)?);
        reports.push(report);
    }
    protect_inputs(output, &allowed.iter().cloned().collect::<Vec<_>>())?;
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
    if let Some(path) = output.output.as_ref().filter(|p| p.exists()) {
        let destination = path.canonicalize()?;
        for input in inputs {
            if input.canonicalize().ok().as_ref() == Some(&destination) {
                bail!("output would overwrite an input file; choose a different output path");
            }
        }
    }
    Ok(())
}
