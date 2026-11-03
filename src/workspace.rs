//! Portable personal Markdown, explicit feedback, and model preference storage.
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const CONTENT_LIMIT: usize = 256 * 1024;
const CONFIG_LIMIT: usize = 64 * 1024;
const NIT_LIMIT: usize = 8192;
pub const MODEL_TASKS: &[&str] = &[
    "analysis",
    "frame-description",
    "video-description",
    "transcription",
    "editing",
    "image-generation",
    "video-generation",
    "image-upscaling",
    "video-upscaling",
    "speech-generation",
    "music-generation",
];

#[derive(Clone, Copy, Debug)]
pub enum ContentKind {
    Styles,
    Skills,
}
impl ContentKind {
    fn directory(self) -> &'static str {
        match self {
            Self::Styles => "styles",
            Self::Skills => "skills",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum ContentCommand {
    /// List personal Markdown and embedded skills, with origin and content hashes.
    List,
    /// Read Markdown as JSON; reading instructions never executes them.
    Read { name: String },
    /// Create a new personal Markdown file; may explicitly shadow an embedded skill.
    Create {
        name: String,
        #[arg(long, required_unless_present = "content", conflicts_with = "content")]
        file: Option<PathBuf>,
        #[arg(long, required_unless_present = "file", conflicts_with = "file")]
        content: Option<String>,
    },
    /// Replace an existing personal file. Embedded skills are read-only.
    Update {
        name: String,
        #[arg(long, required_unless_present = "content", conflicts_with = "content")]
        file: Option<PathBuf>,
        #[arg(long, required_unless_present = "file", conflicts_with = "file")]
        content: Option<String>,
    },
}

const BUILTINS: &[(&str, &str)] = &[
    ("projects", include_str!("../assets/skills/projects.md")),
    (
        "hyperframes",
        include_str!("../assets/skills/hyperframes.md"),
    ),
    ("remotion", include_str!("../assets/skills/remotion.md")),
    ("fcpxml", include_str!("../assets/skills/fcpxml.md")),
    ("premiere", include_str!("../assets/skills/premiere.md")),
    ("ffmpeg", include_str!("../assets/skills/ffmpeg.md")),
    ("takumi", include_str!("../assets/skills/takumi.md")),
    (
        "synchronization",
        include_str!("../assets/skills/synchronization.md"),
    ),
    (
        "composing-tools",
        include_str!("../assets/skills/composing-tools.md"),
    ),
    (
        "character-reference",
        include_str!("../assets/skills/character-reference.md"),
    ),
    (
        "technical-explainer",
        include_str!("../assets/skills/technical-explainer.md"),
    ),
];

pub fn personal_home() -> Result<PathBuf> {
    let path = if let Some(path) = std::env::var_os("SKYWRITE_HOME") {
        PathBuf::from(path)
    } else {
        PathBuf::from(
            std::env::var_os("HOME").context("HOME is unavailable; set absolute SKYWRITE_HOME")?,
        )
        .join(".skywrite")
    };
    if !path.is_absolute() {
        bail!("SKYWRITE_HOME must be an absolute path");
    }
    Ok(path)
}

fn slug(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        bail!("name must contain 1–64 ASCII letters, digits, underscores, or hyphens");
    }
    Ok(())
}

fn check_directory(path: &Path, create: bool) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                bail!("not a regular directory: {}", path.display());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !create {
                return Ok(false);
            }
            fs::create_dir_all(path)
                .with_context(|| format!("create directory {}", path.display()))?;
            // Recheck after creation, including the concurrent-creator case.
            check_directory(path, false)
        }
        Err(error) => Err(error).with_context(|| format!("inspect directory {}", path.display())),
    }
}

fn directory(kind: ContentKind, create: bool) -> Result<PathBuf> {
    let home = personal_home()?;
    if check_directory(&home, create)? {
        check_directory(&home.join(kind.directory()), create)?;
    }
    Ok(home.join(kind.directory()))
}

fn open_regular(path: &Path) -> Result<File> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("not a regular file: {}", path.display());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("not a regular file: {}", path.display());
    }
    Ok(file)
}

fn read_bounded(path: &Path, limit: usize) -> Result<String> {
    let file = open_regular(path)?;
    if file.metadata()?.len() > limit as u64 {
        bail!("file exceeds {limit} bytes: {}", path.display());
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("file exceeds {limit} bytes: {}", path.display());
    }
    String::from_utf8(bytes).with_context(|| format!("file is not UTF-8: {}", path.display()))
}

fn content_input(file: Option<PathBuf>, content: Option<String>) -> Result<String> {
    let content = match (file, content) {
        (Some(file), None) => read_bounded(&file, CONTENT_LIMIT)?,
        (None, Some(content)) => content,
        _ => bail!("provide exactly one of --file or --content"),
    };
    if content.len() > CONTENT_LIMIT {
        bail!("Markdown exceeds {CONTENT_LIMIT} bytes");
    }
    if content.contains('\0') {
        bail!("Markdown must not contain NUL bytes");
    }
    Ok(content)
}

fn exists_regular(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => bail!("not a regular file: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn publish(path: &Path, bytes: &[u8], overwrite: bool) -> Result<()> {
    let parent = path.parent().context("file has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    if overwrite {
        temporary.persist(path).map_err(|error| error.error)?;
    } else {
        crate::io::persist_new(temporary, path)?;
    }
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn entry(
    kind: ContentKind,
    name: &str,
    path: String,
    origin: &str,
    content: &str,
    include_content: bool,
) -> Value {
    let mut value = json!({"name":name,"kind":kind.directory(),"origin":origin,"path":path,
        "bytes":content.len(),"sha256":Sha256::digest(content.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect::<String>()});
    if include_content {
        value["content"] = json!(content);
    }
    value
}

fn read_content(kind: ContentKind, name: &str, include_content: bool) -> Result<Value> {
    slug(name)?;
    let path = directory(kind, false)?.join(format!("{name}.md"));
    if exists_regular(&path)? {
        let content = read_bounded(&path, CONTENT_LIMIT)?;
        return Ok(entry(
            kind,
            name,
            path.display().to_string(),
            "user",
            &content,
            include_content,
        ));
    }
    if let ContentKind::Skills = kind
        && let Some((_, content)) = BUILTINS.iter().find(|(builtin, _)| *builtin == name)
    {
        return Ok(entry(
            kind,
            name,
            format!("builtin://skills/{name}"),
            "builtin",
            content,
            include_content,
        ));
    }
    bail!("{} '{name}' does not exist", kind.directory())
}

pub fn execute_content(kind: ContentKind, command: ContentCommand) -> Result<Value> {
    match command {
        ContentCommand::Read { name } => read_content(kind, &name, true),
        ContentCommand::List => {
            let mut names = BTreeMap::new();
            if let ContentKind::Skills = kind {
                for (name, _) in BUILTINS {
                    names.insert((*name).to_owned(), ());
                }
            }
            let directory = directory(kind, false)?;
            if directory.exists() {
                for item in fs::read_dir(&directory)? {
                    let item = item?;
                    if let Some(name) = item
                        .file_name()
                        .to_str()
                        .and_then(|name| name.strip_suffix(".md"))
                        && slug(name).is_ok()
                    {
                        names.insert(name.to_owned(), ());
                    }
                }
            }
            let items = names
                .keys()
                .map(|name| read_content(kind, name, false))
                .collect::<Result<Vec<_>>>()?;
            Ok(json!({"kind":kind.directory(),"items":items}))
        }
        ContentCommand::Create {
            name,
            file,
            content,
        } => write_content(kind, &name, file, content, false),
        ContentCommand::Update {
            name,
            file,
            content,
        } => write_content(kind, &name, file, content, true),
    }
}

fn write_content(
    kind: ContentKind,
    name: &str,
    file: Option<PathBuf>,
    content: Option<String>,
    overwrite: bool,
) -> Result<Value> {
    slug(name)?;
    // Read the entire input before publishing, preserving it even when paths coincide.
    let content = content_input(file, content)?;
    let path = directory(kind, true)?.join(format!("{name}.md"));
    let exists = exists_regular(&path)?;
    if overwrite && !exists {
        bail!("no personal file '{name}' to update; use create to shadow an embedded skill");
    }
    if !overwrite && exists {
        bail!("personal file '{name}' already exists; use update");
    }
    publish(&path, content.as_bytes(), overwrite)?;
    Ok(entry(
        kind,
        name,
        path.display().to_string(),
        "user",
        &content,
        true,
    ))
}

/// Save only the explicitly provided text; no environment, credentials, or media are captured.
pub fn record_nit(description: &str) -> Result<Value> {
    if description.trim().is_empty() || description.len() > NIT_LIMIT || description.contains('\0')
    {
        bail!("nit description must be nonempty UTF-8, at most {NIT_LIMIT} bytes, without NUL");
    }
    let home = personal_home()?;
    check_directory(&home, true)?;
    let directory = home.join("nits");
    check_directory(&directory, true)?;
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let value = json!({"version":1,"app_version":env!("CARGO_PKG_VERSION"),
        "created_unix_seconds":elapsed.as_secs(),"created_unix_nanoseconds":elapsed.subsec_nanos(),"description":description});
    let mut bytes = serde_json::to_vec_pretty(&value)?;
    bytes.push(b'\n');
    // A random final name plus no-clobber publication remains unique across processes.
    let mut temporary = tempfile::Builder::new()
        .prefix(".nit-")
        .tempfile_in(&directory)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    let suffix = temporary
        .path()
        .file_name()
        .context("temporary name missing")?
        .to_string_lossy()
        .trim_start_matches('.')
        .to_owned();
    let path = directory.join(format!(
        "{}-{}-{suffix}.json",
        elapsed.as_secs(),
        elapsed.subsec_nanos()
    ));
    crate::io::persist_new(temporary, &path)?;
    File::open(&directory)?.sync_all()?;
    Ok(json!({"saved":true,"path":path.display().to_string(),"nit":value}))
}

fn validate_task(task: &str) -> Result<()> {
    if !MODEL_TASKS.contains(&task) {
        bail!("unknown task '{task}'; expected {}", MODEL_TASKS.join(", "));
    }
    Ok(())
}
fn validate_model(model: &str) -> Result<()> {
    if model.is_empty()
        || model.len() > 256
        || model.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        bail!("model ID must be 1–256 bytes without whitespace or controls");
    }
    Ok(())
}
fn read_config(path: &Path) -> Result<Value> {
    if !exists_regular(path)? {
        return Ok(json!({"version":1,"model_defaults":{}}));
    }
    let value: Value = serde_json::from_str(&read_bounded(path, CONFIG_LIMIT)?)
        .context("invalid personal config JSON")?;
    if !value.is_object() {
        bail!("personal config must be a JSON object");
    }
    defaults_from(&value)?;
    Ok(value)
}
fn defaults_from(config: &Value) -> Result<BTreeMap<String, String>> {
    let mut defaults = BTreeMap::new();
    if let Some(value) = config.get("model_defaults") {
        let object = value
            .as_object()
            .context("config model_defaults must be an object")?;
        for (task, model) in object {
            validate_task(task)?;
            let model = model.as_str().context("config model IDs must be strings")?;
            validate_model(model)?;
            defaults.insert(task.clone(), model.to_owned());
        }
    }
    Ok(defaults)
}

pub fn model_defaults() -> Result<BTreeMap<String, String>> {
    let home = personal_home()?;
    check_directory(&home, false)?;
    defaults_from(&read_config(&home.join("config.json"))?)
}
pub fn get_default_model(task: &str) -> Result<Option<String>> {
    let task = crate::models::canonical_task(task)?;
    validate_task(task)?;
    Ok(model_defaults()?.remove(task))
}
pub fn set_default_model(task: &str, model: &str) -> Result<Value> {
    let task = crate::models::canonical_task(task)?;
    validate_task(task)?;
    validate_model(model)?;
    change_default(task, Some(model))
}
pub fn unset_default_model(task: &str) -> Result<Value> {
    let task = crate::models::canonical_task(task)?;
    validate_task(task)?;
    change_default(task, None)
}
fn lock_config() -> Result<(PathBuf, File)> {
    let home = personal_home()?;
    check_directory(&home, true)?;
    let path = home.join("config.json");
    let lock_path = home.join("config.lock");
    // The stable lock inode is never renamed. No other locks are acquired while held.
    if lock_path.exists() {
        exists_regular(&lock_path)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let lock = options
        .open(&lock_path)
        .context("open personal config lock")?;
    if !lock.metadata()?.is_file() {
        bail!("config lock must be a regular file");
    }
    lock.lock().context("lock personal config")?;
    Ok((path, lock))
}

fn change_default(task: &str, model: Option<&str>) -> Result<Value> {
    let (path, _lock) = lock_config()?;
    let mut config = read_config(&path)?;
    let mut defaults = defaults_from(&config)?;
    match model {
        Some(model) => {
            defaults.insert(task.to_owned(), model.to_owned());
        }
        None => {
            defaults.remove(task);
        }
    }
    config["model_defaults"] = serde_json::to_value(&defaults)?;
    let mut bytes = serde_json::to_vec_pretty(&config)?;
    bytes.push(b'\n');
    if bytes.len() > CONFIG_LIMIT {
        bail!("personal config exceeds {CONFIG_LIMIT} bytes");
    }
    publish(&path, &bytes, true)?;
    Ok(
        json!({"task":task,"model":model,"model_defaults":defaults,"path":path.display().to_string()}),
    )
}

/// Merge only missing starter preferences under the same stable lock used by
/// individual setters. A dry run reads an atomic snapshot without creating files.
pub fn initialize_default_models(
    starters: &BTreeMap<String, String>,
    dry_run: bool,
) -> Result<Value> {
    for (task, model) in starters {
        validate_task(task)?;
        validate_model(model)?;
    }
    let (path, _lock) = if dry_run {
        let home = personal_home()?;
        check_directory(&home, false)?;
        (home.join("config.json"), None)
    } else {
        let (path, lock) = lock_config()?;
        (path, Some(lock))
    };
    let existed = exists_regular(&path)?;
    let mut config = read_config(&path)?;
    let preserved = defaults_from(&config)?;
    let mut defaults = preserved.clone();
    let mut added = BTreeMap::new();
    for (task, model) in starters {
        if !defaults.contains_key(task) {
            defaults.insert(task.clone(), model.clone());
            added.insert(task.clone(), model.clone());
        }
    }
    let would_change = !existed || !added.is_empty();
    if would_change {
        config["model_defaults"] = serde_json::to_value(&defaults)?;
        let mut bytes = serde_json::to_vec_pretty(&config)?;
        bytes.push(b'\n');
        if bytes.len() > CONFIG_LIMIT {
            bail!("personal config exceeds {CONFIG_LIMIT} bytes");
        }
        if !dry_run {
            publish(&path, &bytes, true)?;
        }
    }
    Ok(
        json!({"initialized":!dry_run,"dry_run":dry_run,"config_path":path,
        "created":!dry_run && !existed,"changed":!dry_run && would_change,
        "would_change":would_change,"added":added,"preserved":preserved,"model_defaults":defaults}),
    )
}
