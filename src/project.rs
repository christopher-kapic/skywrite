//! Declarative generation with durable admission. Lock order: project -> job -> storage.
//! Scoped workers are always joined before the project lock is released.
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;

const LIMIT: usize = 1024 * 1024;
const MAX_NODES: usize = 256;
// Match generation::image_bytes; project file references are images, never media videos.
const INPUT_IMAGE_LIMIT: u64 = 16 * 1024 * 1024;
const INPUT_TOTAL_LIMIT: u64 = 256 * 1024 * 1024;
#[cfg(test)]
thread_local! { static INPUT_HASH_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
#[derive(Debug, Args)]
pub struct RunArgs {
    pub manifest: PathBuf,
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=8))]
    pub concurrency: u8,
    /// Continue independent nodes after an error; dependencies still require completion.
    #[arg(long)]
    pub keep_going: bool,
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
    /// Use cached model metadata. Generation still requires the network.
    #[arg(long)]
    pub offline: bool,
    /// Per video node retrieval deadline; unfinished jobs remain resumable.
    #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(0..=1800))]
    pub wait: u64,
}
#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    Schema,
    /// Write an example manifest without making requests or replacing a file.
    Init {
        manifest: PathBuf,
    },
    /// Structural validation is offline; --check-models also uses generation dry runs.
    Validate {
        manifest: PathBuf,
        #[arg(long)]
        check_models: bool,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        #[arg(long)]
        offline: bool,
    },
    Run(RunArgs),
    Resume(RunArgs),
    /// Read local aggregate state and verify completed output hashes.
    Status {
        manifest: PathBuf,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub output_root: PathBuf,
    pub nodes: Vec<Node>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub id: String,
    pub output: PathBuf,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Change explicitly for a paid retake, using a new manifest/run identity.
    #[serde(default)]
    pub take: Option<String>,
    pub action: Action,
    #[serde(default)]
    pub shot: Option<Shot>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shot {
    pub size: String,
    pub angle: String,
    pub motion: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Reference {
    File { file: PathBuf },
    Node { node: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Image {
        prompt: String,
        #[serde(default)]
        references: Vec<Reference>,
        #[serde(default)]
        resolution: Option<String>,
        #[serde(default)]
        aspect_ratio: Option<String>,
        #[serde(default)]
        seed: Option<u64>,
    },
    Video {
        prompt: String,
        #[serde(default)]
        references: Vec<Reference>,
        #[serde(default)]
        first_frame: Option<Reference>,
        #[serde(default)]
        last_frame: Option<Reference>,
        #[serde(default)]
        duration: Option<u32>,
        #[serde(default)]
        resolution: Option<String>,
        #[serde(default)]
        aspect_ratio: Option<String>,
        #[serde(default)]
        seed: Option<u64>,
        #[serde(default)]
        audio: Option<bool>,
    },
    Speech {
        text: String,
        #[serde(default)]
        voice: Option<String>,
        #[serde(default)]
        instructions: Option<String>,
        #[serde(default)]
        speed: Option<f64>,
    },
    Music {
        prompt: String,
        #[serde(default)]
        instrumental: bool,
        #[serde(default)]
        reference: Option<Reference>,
    },
}
impl Action {
    fn task(&self) -> &'static str {
        match self {
            Self::Image { .. } => "image-generation",
            Self::Video { .. } => "video-generation",
            Self::Speech { .. } => "speech-generation",
            Self::Music { .. } => "music-generation",
        }
    }
    fn references(&self) -> Vec<&Reference> {
        match self {
            Self::Image { references, .. } => references.iter().collect(),
            Self::Video {
                references,
                first_frame,
                last_frame,
                ..
            } => references
                .iter()
                .chain(first_frame.iter())
                .chain(last_frame.iter())
                .collect(),
            Self::Music { reference, .. } => reference.iter().collect(),
            Self::Speech { .. } => vec![],
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeState {
    model: String,
    status: String,
    planned_id: Option<String>,
    output_sha256: Option<String>,
    error: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    manifest_sha256: String,
    inputs: BTreeMap<PathBuf, String>,
    nodes: BTreeMap<String, NodeState>,
}
struct Loaded {
    path: PathBuf,
    base: PathBuf,
    manifest: Manifest,
    hash: String,
}
fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("inspecting {}", path.display())),
    }
}
fn regular(path: &Path, create: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .mode(0o600);
    }
    let f = options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    if !f.metadata()?.is_file() {
        bail!("project files must be regular non-symlink files");
    }
    Ok(f)
}
fn bytes(path: &Path) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    regular(path, false)?
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut out)?;
    if out.len() > LIMIT {
        bail!("project file exceeds one MiB");
    }
    Ok(out)
}
fn relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("paths must be nonempty relative paths without . or ..");
    }
    Ok(())
}
// Reject symlink ancestors, including dangling links, even before an output exists.
fn safe_path(base: &Path, relative_path: &Path) -> Result<PathBuf> {
    relative(relative_path)?;
    let mut path = base.to_owned();
    for c in relative_path.components() {
        path.push(c.as_os_str());
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                bail!("symlink in project path {}", path.display())
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e).with_context(|| format!("inspecting {}", path.display())),
        }
    }
    Ok(path)
}
fn load(path: &Path) -> Result<Loaded> {
    // Resolve the parent, retaining the manifest filename to reject a symlink itself.
    let absolute = std::env::current_dir()?.join(path);
    let base = absolute
        .parent()
        .context("manifest has no parent")?
        .canonicalize()?;
    let path = base.join(absolute.file_name().context("manifest has no name")?);
    let raw = bytes(&path)?;
    let manifest: Manifest = serde_json::from_slice(&raw).context("invalid project manifest")?;
    let loaded = Loaded {
        path,
        base,
        manifest,
        hash: crate::openrouter::cache_key(&[&raw]),
    };
    validate(&loaded)?;
    Ok(loaded)
}
fn check_text(s: &str, max: usize, label: &str) -> Result<()> {
    if s.trim().is_empty() || s.len() > max || s.contains('\0') {
        bail!("{label} must contain 1..{max} bytes without NUL");
    }
    Ok(())
}
fn dependencies(node: &Node) -> BTreeSet<&str> {
    node.depends_on
        .iter()
        .map(String::as_str)
        .chain(
            node.action
                .references()
                .into_iter()
                .filter_map(|r| match r {
                    Reference::Node { node } => Some(node.as_str()),
                    _ => None,
                }),
        )
        .collect()
}
fn output(l: &Loaded, n: &Node) -> Result<PathBuf> {
    safe_path(&l.base, &l.manifest.output_root.join(&n.output))
}
fn validate(l: &Loaded) -> Result<()> {
    let m = &l.manifest;
    if m.version != 1 || m.nodes.is_empty() || m.nodes.len() > MAX_NODES {
        bail!("project version must be 1 with 1..{MAX_NODES} nodes");
    }
    relative(&m.output_root)?;
    safe_path(&l.base, &m.output_root)?;
    let mut ids = BTreeSet::new();
    let mut outputs = BTreeSet::new();
    for n in &m.nodes {
        if n.id.is_empty()
            || n.id.len() > 64
            || !n
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || !ids.insert(&n.id)
        {
            bail!("node IDs must be unique 1..64 ASCII letters, digits, underscores or hyphens");
        }
        relative(&n.output)?;
        let out = output(l, n)?;
        if out == l.path || !outputs.insert(out) {
            bail!("node outputs must be distinct and preserve the manifest");
        }
        let ext = n
            .output
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let ok = match &n.action {
            Action::Image { .. } => matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp"),
            Action::Video { .. } => ext == "mp4",
            _ => matches!(ext.as_str(), "mp3" | "wav"),
        };
        if !ok {
            bail!("output extension does not match action for {}", n.id);
        }
        if n.depends_on.len() > MAX_NODES || n.action.references().len() > 10 {
            bail!("too many dependencies or references");
        }
        if let Some(take) = &n.take {
            check_text(take, 256, "take")?;
        }
        if let Some(model) = &n.model {
            crate::models::resolve_model(n.action.task(), Some(model))?;
        }
        match &n.action {
            Action::Image { prompt, .. }
            | Action::Music { prompt, .. }
            | Action::Video { prompt, .. } => check_text(prompt, 32 * 1024, "prompt")?,
            Action::Speech {
                text,
                instructions,
                voice,
                speed,
            } => {
                check_text(text, 8192, "speech text")?;
                if let Some(s) = instructions {
                    check_text(s, 4096, "instructions")?;
                }
                if let Some(s) = voice {
                    check_text(s, 256, "voice")?;
                    if s.chars().any(char::is_control) {
                        bail!("voice contains controls");
                    }
                }
                if speed.is_some_and(|s| !s.is_finite() || !(0.5..=2.0).contains(&s)) {
                    bail!("speech speed must be 0.5..2.0");
                }
            }
        }
        if let Action::Video {
            references,
            first_frame,
            last_frame,
            ..
        } = &n.action
            && !references.is_empty()
            && (first_frame.is_some() || last_frame.is_some())
        {
            bail!("video references and frame inputs conflict");
        }
        if let Some(shot) = &n.shot {
            if !matches!(n.action, Action::Video { .. }) {
                bail!("shot labels apply only to video nodes");
            }
            for s in [&shot.size, &shot.angle, &shot.motion] {
                check_text(s, 128, "shot label")?;
            }
        }
    }
    for n in &m.nodes {
        for d in dependencies(n) {
            if d == n.id || !ids.contains(&d.to_owned()) {
                bail!("unknown/self dependency {d} on {}", n.id);
            }
        }
        for r in n.action.references() {
            match r {
                Reference::File { file } => {
                    let p = safe_path(&l.base, file)?;
                    regular(&p, false)?;
                    if outputs.contains(&p) {
                        bail!("local inputs must not alias outputs; use a node reference");
                    }
                }
                Reference::Node { node } => {
                    if !m
                        .nodes
                        .iter()
                        .any(|n| &n.id == node && matches!(n.action, Action::Image { .. }))
                    {
                        bail!("reference node {node} must produce an image");
                    }
                }
            }
        }
    }
    let mut visited = BTreeSet::new();
    while visited.len() < m.nodes.len() {
        let before = visited.len();
        for n in &m.nodes {
            if dependencies(n).iter().all(|d| visited.contains(*d)) {
                visited.insert(n.id.as_str());
            }
        }
        if before == visited.len() {
            bail!("project dependencies contain a cycle");
        }
    }
    input_inventory(l)?;
    Ok(())
}
fn warnings(l: &Loaded) -> Vec<String> {
    let mut seen = BTreeMap::new();
    let mut out = vec![];
    for n in &l.manifest.nodes {
        if let Some(s) = &n.shot {
            let key = (&s.size, &s.angle, &s.motion);
            let count = seen.entry(key).or_insert(0);
            *count += 1;
            if *count == 3 {
                out.push(format!("Three or more video nodes share declared framing ({}, {}, {}). Metadata labels only; review actual visuals. No retake is automatic.",s.size,s.angle,s.motion));
            }
        }
    }
    out
}
// Metadata admission and canonical deduplication precede ANY input payload reads.
// Keep logical manifest keys in state; only the expensive content read is shared.
fn input_inventory(l: &Loaded) -> Result<BTreeMap<PathBuf, PathBuf>> {
    let mut logical = BTreeMap::new();
    let mut unique = BTreeSet::new();
    let mut total = 0u64;
    for n in &l.manifest.nodes {
        for r in n.action.references() {
            if let Reference::File { file } = r {
                if logical.contains_key(file) {
                    continue;
                }
                let path = safe_path(&l.base, file)?;
                let canonical = path.canonicalize()?;
                if !canonical.starts_with(&l.base) {
                    bail!("project input resolves outside manifest directory");
                }
                // Open the original checked path, retaining no symlink-following opener.
                let length = regular(&path, false)?.metadata()?.len();
                if length > INPUT_IMAGE_LIMIT {
                    bail!("project image input exceeds 16 MiB: {}", file.display());
                }
                if unique.insert(canonical.clone()) {
                    total = total
                        .checked_add(length)
                        .context("project input size overflow")?;
                    if total > INPUT_TOTAL_LIMIT {
                        bail!("unique project image inputs exceed 256 MiB");
                    }
                }
                logical.insert(file.clone(), canonical);
            }
        }
    }
    Ok(logical)
}
fn input_digest(reader: impl Read, limit: u64) -> Result<(String, u64)> {
    #[cfg(test)]
    struct Observed<R>(R);
    #[cfg(test)]
    impl<R: Read> Read for Observed<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let count = self.0.read(buf)?;
            INPUT_HASH_BYTES.with(|bytes| bytes.set(bytes.get() + count as u64));
            Ok(count)
        }
    }
    #[cfg(test)]
    let reader = Observed(reader);
    crate::media::hash_reader_bounded(reader, limit)
}
fn inputs(l: &Loaded) -> Result<BTreeMap<PathBuf, String>> {
    let logical = input_inventory(l)?;
    let mut hashes = BTreeMap::new();
    let mut total = 0u64;
    for path in logical.values() {
        if !hashes.contains_key(path) {
            let input = regular(path, false)?;
            if input.metadata()?.len() > INPUT_IMAGE_LIMIT {
                bail!("project image input exceeds 16 MiB: {}", path.display());
            }
            let limit = INPUT_IMAGE_LIMIT.min(INPUT_TOTAL_LIMIT - total);
            let (hash, length) = input_digest(input, limit)?;
            total += length;
            hashes.insert(path.clone(), hash);
        }
    }
    logical
        .into_iter()
        .map(|(file, path)| Ok((file, hashes[&path].clone())))
        .collect()
}
fn state_dir(l: &Loaded) -> Result<PathBuf> {
    let home = crate::workspace::personal_home()?;
    secure_directory(&home)?;
    let projects = home.join("projects");
    secure_directory(&projects)?;
    let key = crate::openrouter::cache_key(&[b"project-v1", l.path.as_os_str().as_encoded_bytes()]);
    let root = projects.join(key);
    secure_directory(&root)?;
    Ok(root)
}
fn secure_directory(path: &Path) -> Result<()> {
    if present(path)? {
        let m = fs::symlink_metadata(path)?;
        if !m.is_dir() || m.file_type().is_symlink() {
            bail!("project state directory must not be a symlink");
        }
    } else {
        fs::create_dir_all(path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    crate::generation::sync_directory_chain(path)?;
    Ok(())
}
fn lock(root: &Path) -> Result<File> {
    let f = regular(&root.join("lock"), true)?;
    f.try_lock()
        .context("project is already locked by another operation")?;
    Ok(f)
}
fn save(root: &Path, state: &State) -> Result<()> {
    let encoded = serde_json::to_vec_pretty(state)?;
    if encoded.len() > LIMIT {
        bail!("project state exceeds one MiB");
    }
    crate::io::atomic_write(&root.join("state.json"), &encoded, true)?;
    File::open(root)?.sync_all()?;
    Ok(())
}
fn read_state(l: &Loaded, root: &Path) -> Result<Option<State>> {
    if !present(&root.join("state.json"))? {
        return Ok(None);
    }
    let state: State = serde_json::from_slice(&bytes(&root.join("state.json"))?)?;
    if state.version != 1 || state.manifest_sha256 != l.hash {
        bail!(
            "manifest changed since this run; preserve it and use a new manifest path for an intentional new run"
        );
    }
    if state.nodes.len() != l.manifest.nodes.len()
        || l.manifest
            .nodes
            .iter()
            .any(|n| !state.nodes.contains_key(&n.id))
    {
        bail!("project state node inventory mismatch");
    }
    if state.inputs != inputs(l)? {
        bail!(
            "project input changed since this run; preserve the original inputs or create a new manifest/run"
        );
    }
    for n in &l.manifest.nodes {
        let s = &state.nodes[&n.id];
        crate::models::resolve_model(n.action.task(), Some(&s.model))?;
        if n.model.as_ref().is_some_and(|m| m != &s.model) {
            bail!("frozen model mismatch");
        }
        if !matches!(
            s.status.as_str(),
            "pending" | "running" | "completed" | "failed" | "uncertain" | "waiting"
        ) {
            bail!("invalid project node state");
        }
        if let Some(id) = &s.planned_id
            && (id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            bail!("invalid planned job ID");
        }
        if s.status != "pending" && s.planned_id.is_none() {
            bail!("started project node lacks durable job identity");
        }
        if s.status == "completed" && s.output_sha256.is_none() {
            bail!("completed project node lacks output hash");
        }
    }
    Ok(Some(state))
}
fn reference_path(l: &Loaded, r: &Reference, placeholder: Option<&Path>) -> Result<PathBuf> {
    match r {
        Reference::File { file } => safe_path(&l.base, file),
        Reference::Node { node } => {
            if let Some(p) = placeholder {
                Ok(p.to_owned())
            } else {
                output(
                    l,
                    l.manifest
                        .nodes
                        .iter()
                        .find(|n| &n.id == node)
                        .context("missing reference node")?,
                )
            }
        }
    }
}
fn command(
    l: &Loaded,
    n: &Node,
    model: &str,
    args: &RunArgs,
    dry_run: bool,
    placeholder: Option<&Path>,
) -> Result<crate::generation::GenerateCommand> {
    use crate::generation::{Common, GenerateCommand, ImageArgs, VideoArgs};
    let common = Common {
        model: Some(model.into()),
        output: Some(output(l, n)?),
        overwrite: false,
        cache_dir: args.cache_dir.clone(),
        offline: args.offline,
        dry_run,
        take: n.take.clone(),
    };
    let refs = |refs: &[Reference]| {
        refs.iter()
            .map(|r| reference_path(l, r, placeholder))
            .collect::<Result<Vec<_>>>()
    };
    Ok(match &n.action {
        Action::Image {
            prompt,
            references,
            resolution,
            aspect_ratio,
            seed,
        } => GenerateCommand::Image(ImageArgs {
            prompt: prompt.clone(),
            reference: refs(references)?,
            resolution: resolution.clone(),
            aspect_ratio: aspect_ratio.clone(),
            seed: *seed,
            common,
        }),
        Action::Video {
            prompt,
            references,
            first_frame,
            last_frame,
            duration,
            resolution,
            aspect_ratio,
            seed,
            audio,
        } => GenerateCommand::Video(VideoArgs {
            prompt: prompt.clone(),
            reference: refs(references)?,
            first_frame: first_frame
                .as_ref()
                .map(|r| reference_path(l, r, placeholder))
                .transpose()?,
            last_frame: last_frame
                .as_ref()
                .map(|r| reference_path(l, r, placeholder))
                .transpose()?,
            duration: *duration,
            resolution: resolution.clone(),
            aspect_ratio: aspect_ratio.clone(),
            seed: *seed,
            audio: *audio,
            wait: args.wait,
            common,
        }),
        Action::Speech {
            text,
            voice,
            instructions,
            speed,
        } => GenerateCommand::Speech(crate::audio_generation::SpeechArgs {
            text: Some(text.clone()),
            file: None,
            voice: voice.clone(),
            instructions: instructions.clone(),
            speed: *speed,
            paragraph_pause: 0.3,
            single_request: true,
            common,
        }),
        Action::Music {
            prompt,
            instrumental,
            reference,
        } => GenerateCommand::Music(crate::audio_generation::MusicArgs {
            prompt: prompt.clone(),
            instrumental: *instrumental,
            reference: reference
                .as_ref()
                .map(|r| reference_path(l, r, placeholder))
                .transpose()?,
            common,
        }),
    })
}
// A valid tiny image lets the existing adapters check model capabilities before any
// node is admitted, even when its real dependency image has not been generated yet.
fn preflight(l: &Loaded, state: &State, args: &RunArgs) -> Result<()> {
    use base64::Engine;
    let temp = tempfile::Builder::new().suffix(".png").tempfile()?;
    fs::write(temp.path(),base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aPeYAAAAASUVORK5CYII=")?)?;
    for n in &l.manifest.nodes {
        crate::generation::execute_generate(command(
            l,
            n,
            &state.nodes[&n.id].model,
            args,
            true,
            Some(temp.path()),
        )?)
        .with_context(|| format!("validating node {}", n.id))?;
    }
    Ok(())
}
fn initial(l: &Loaded) -> Result<State> {
    let nodes = l
        .manifest
        .nodes
        .iter()
        .map(|n| {
            Ok((
                n.id.clone(),
                NodeState {
                    model: crate::models::resolve_model(n.action.task(), n.model.as_deref())?,
                    status: "pending".into(),
                    planned_id: None,
                    output_sha256: None,
                    error: None,
                },
            ))
        })
        .collect::<Result<_>>()?;
    Ok(State {
        version: 1,
        manifest_sha256: l.hash.clone(),
        inputs: inputs(l)?,
        nodes,
    })
}
fn verify_outputs(l: &Loaded, state: &State) -> Result<()> {
    for n in &l.manifest.nodes {
        let s = &state.nodes[&n.id];
        let path = output(l, n)?;
        if s.status == "completed" {
            if crate::media::file_sha256(&path)?
                != s.output_sha256.as_deref().context("missing output hash")?
            {
                bail!("completed output changed: {}", path.display());
            }
        } else if present(&path)? {
            bail!(
                "unrecorded output exists: {}; refusing to overwrite or trust it",
                path.display()
            );
        }
    }
    Ok(())
}
// Bind the digests of the very buffers serialized into the planned request to
// frozen authorization. The worker's planned ID check then protects the next read.
fn verify_plan_inputs(l: &Loaded, n: &Node, state: &State, plan: &Value) -> Result<()> {
    let mut expected = BTreeMap::new();
    for reference in n.action.references() {
        let hash = match reference {
            Reference::File { file } => state
                .inputs
                .get(file)
                .context("missing frozen input hash")?,
            Reference::Node { node } => state.nodes[node]
                .output_sha256
                .as_ref()
                .context("missing completed dependency hash")?,
        };
        expected.insert(reference_path(l, reference, None)?.canonicalize()?, hash);
    }
    let references = plan["references"]
        .as_array()
        .context("plan lacks reference audit")?;
    let mut seen = BTreeSet::new();
    for reference in references {
        let path = PathBuf::from(reference["path"].as_str().context("reference lacks path")?);
        let hash = expected
            .get(&path)
            .context("plan contains an unauthorized reference")?;
        if reference["sha256"].as_str() != Some(hash.as_str()) {
            bail!(
                "project planned input differs from frozen input: {}",
                path.display()
            );
        }
        seen.insert(path);
    }
    if seen.len() != expected.len() {
        bail!("plan omits a frozen reference");
    }
    Ok(())
}
fn repair_completed_outputs(l: &Loaded, state: &State) -> Result<()> {
    for n in &l.manifest.nodes {
        let s = &state.nodes[&n.id];
        let path = output(l, n)?;
        if s.status == "completed" && !present(&path)? {
            crate::generation::restore_project_publication(
                s.planned_id
                    .as_deref()
                    .context("missing completed job ID")?,
                &path,
                s.output_sha256
                    .as_deref()
                    .context("missing completed output hash")?,
            )?;
        }
    }
    Ok(())
}
#[cfg(test)]
thread_local! {
    static BEFORE_PLAN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
fn summary(l: &Loaded, state: Option<&State>, root: &Path) -> Result<Value> {
    let mut counts = BTreeMap::<&str, usize>::new();
    let mut nodes = vec![];
    for n in &l.manifest.nodes {
        let s = state.and_then(|s| s.nodes.get(&n.id));
        let status = s.map_or("pending", |s| s.status.as_str());
        *counts.entry(status).or_default() += 1;
        nodes.push(json!({"id":n.id,"status":status,"model":s.map(|s|&s.model),"job_id":s.and_then(|s|s.planned_id.as_ref()),"output":output(l,n)?,"output_sha256":s.and_then(|s|s.output_sha256.as_ref()),"error":s.and_then(|s|s.error.as_ref())}));
    }
    Ok(
        json!({"version":1,"manifest":l.path,"manifest_sha256":l.hash,"state_file":root.join("state.json"),"complete":state.is_some() && counts.get("completed")==Some(&l.manifest.nodes.len()),"counts":counts,"nodes":nodes,"warnings":warnings(l),"retakes":"Explicit new take and new manifest path only; resume never repeats an uncertain submission."}),
    )
}
#[derive(Clone, Default)]
struct Engine {
    #[cfg(test)]
    base: Option<String>,
}
impl Engine {
    fn generate(&self, c: crate::generation::GenerateCommand, expected: &str) -> Result<Value> {
        #[cfg(test)]
        if let Some(base) = &self.base {
            return crate::generation::with_test_transport(base, || {
                crate::generation::execute_generate_planned(c, expected)
            });
        }
        crate::generation::execute_generate_planned(c, expected)
    }
    fn resume(&self, c: crate::generation::JobsCommand) -> Result<Value> {
        #[cfg(test)]
        if let Some(base) = &self.base {
            return crate::generation::with_test_transport(base, || {
                crate::generation::execute_jobs(c)
            });
        }
        crate::generation::execute_jobs(c)
    }
}
fn recover(l: &Loaded, n: &Node, s: &NodeState, args: &RunArgs, engine: &Engine) -> Result<Value> {
    let id = s
        .planned_id
        .as_ref()
        .context("node has no durable planned ID")?;
    // Read status first: a missing receipt never permits a new generation.
    let receipt = crate::generation::execute_jobs(crate::generation::JobsCommand::Status {
        id: id.clone(),
        refresh: false,
    })
    .context(
        "started node has no readable job receipt; outcome uncertain, refusing a new submission",
    )?;
    if receipt["model"] != s.model || receipt["task"] != n.action.task() {
        bail!("job receipt disagrees with frozen node model/task");
    }
    let path = output(l, n)?;
    if present(&path)? {
        // A crash may have published bytes before recording project completion.
        // The publisher may convert formats; reproduce it privately before adopting.
        return crate::generation::verify_project_publication(id, &path);
    }
    engine.resume(crate::generation::JobsCommand::Resume {
        id: id.clone(),
        output: Some(path),
        overwrite: false,
        wait: args.wait,
        remote_id: None,
    })
}
fn run(args: RunArgs, resume: bool, engine: Engine) -> Result<Value> {
    if !(1..=8).contains(&args.concurrency) || args.wait > 1800 {
        bail!("concurrency must be 1..8 and wait at most 1800 seconds");
    }
    let l = load(&args.manifest)?;
    let root = state_dir(&l)?;
    let _lock = lock(&root)?;
    let existing = read_state(&l, &root)?;
    if resume && existing.is_none() {
        bail!("no existing project run; use project run first");
    }
    let new = existing.is_none();
    let mut state = match existing {
        Some(s) => s,
        None => initial(&l)?,
    };
    repair_completed_outputs(&l, &state)?;
    if state.nodes.values().all(|s| s.status == "completed") {
        verify_outputs(&l, &state)?;
        return summary(&l, Some(&state), &root);
    }
    if new {
        verify_outputs(&l, &state)?;
        preflight(&l, &state, &args)?;
        save(&root, &state)?;
    } else {
        // Verify only known completed outputs; started nodes may have a publication
        // awaiting crash recovery, which is checked against its job receipt below.
        for n in &l.manifest.nodes {
            if state.nodes[&n.id].status == "completed" {
                let s = &state.nodes[&n.id];
                if crate::media::file_sha256(&output(&l, n)?)?
                    != s.output_sha256.as_deref().context("missing output hash")?
                {
                    bail!("completed output changed for {}", n.id);
                }
            }
        }
    }
    let result = std::thread::scope(|scope| -> Result<()> {
        let (tx, rx) = mpsc::channel();
        let mut active = 0usize;
        let mut attempted = BTreeSet::new();
        let mut stopped = false;
        let mut internal_error = None;
        loop {
            for n in &l.manifest.nodes {
                if active >= args.concurrency as usize
                    || internal_error.is_some()
                    || (stopped && !args.keep_going)
                {
                    break;
                }
                let s = &state.nodes[&n.id];
                if s.status == "completed" || attempted.contains(&n.id) {
                    continue;
                }
                if !dependencies(n)
                    .iter()
                    .all(|d| state.nodes[*d].status == "completed")
                {
                    continue;
                }
                if s.status == "failed" {
                    stopped = true;
                    attempted.insert(n.id.clone());
                    continue;
                }
                let recovering = s.status != "pending";
                if !recovering {
                    let path = output(&l, n)?;
                    if present(&path)? {
                        bail!("output already exists for {}; refusing paid work", n.id);
                    }
                    // Recheck all source hashes immediately before every first admission.
                    if inputs(&l)? != state.inputs {
                        bail!("project inputs changed during run");
                    }
                    #[cfg(test)]
                    BEFORE_PLAN.with(|hook| {
                        if let Some(hook) = hook.borrow_mut().take() {
                            hook();
                        }
                    });
                    let plan = crate::generation::execute_generate(command(
                        &l, n, &s.model, &args, true, None,
                    )?)?;
                    verify_plan_inputs(&l, n, &state, &plan)?;
                    let id = plan["id"]
                        .as_str()
                        .context("generation dry run lacks deterministic ID")?
                        .to_owned();
                    let s = state.nodes.get_mut(&n.id).unwrap();
                    if s.planned_id
                        .as_ref()
                        .is_some_and(|previous| previous != &id)
                    {
                        bail!("planned job identity changed for {}", n.id);
                    }
                    s.planned_id = Some(id);
                    s.status = "running".into();
                    s.error = None;
                    save(&root, &state)?; // durable admission precedes worker/provider effects
                }
                let s = state.nodes[&n.id].clone();
                let tx = tx.clone();
                let engine = engine.clone();
                let l = &l;
                let args = &args;
                let id = n.id.clone();
                attempted.insert(id.clone());
                active += 1;
                scope.spawn(move || {
                    let work = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if recovering {
                            recover(l, n, &s, args, &engine)
                        } else {
                            engine.generate(
                                command(l, n, &s.model, args, false, None)?,
                                s.planned_id.as_deref().context("missing planned ID")?,
                            )
                        }
                    }))
                    .unwrap_or_else(|_| {
                        Err(anyhow::anyhow!(
                            "generation worker panicked; outcome uncertain"
                        ))
                    });
                    let _ = tx.send((id, work));
                });
            }
            if active == 0 {
                break;
            }
            let (id, value) = rx.recv().context("project worker channel closed")?;
            active -= 1;
            let n = l.manifest.nodes.iter().find(|n| n.id == id).unwrap();
            let s = state.nodes.get_mut(&id).unwrap();
            match value {
                Ok(value) => {
                    if value["id"].as_str() != s.planned_id.as_deref() {
                        s.status = "uncertain".into();
                        s.error = Some("generation result changed planned identity".into());
                        stopped = true;
                    } else if value["status"] == "completed" {
                        match crate::media::file_sha256(&output(&l, n)?) {
                            Ok(hash) => {
                                s.status = "completed".into();
                                s.output_sha256 = Some(hash);
                                s.error = None;
                            }
                            Err(e) => {
                                s.status = "uncertain".into();
                                s.error = Some(format!("completed job output unavailable: {e:#}"));
                                stopped = true;
                            }
                        }
                    } else {
                        let status = value["status"].as_str().unwrap_or("uncertain");
                        s.status = if matches!(status, "failed" | "rejected") {
                            "failed"
                        } else if matches!(
                            status,
                            "pending" | "processing" | "queued" | "in_progress"
                        ) {
                            "waiting"
                        } else {
                            "uncertain"
                        }
                        .into();
                        s.error = value["error"].as_str().map(str::to_owned);
                        if s.status != "waiting" {
                            stopped = true;
                        }
                    }
                }
                Err(e) => {
                    s.status = "uncertain".into();
                    s.error = Some(format!("{e:#}"));
                    stopped = true;
                }
            }
            if let Err(e) = save(&root, &state) {
                stopped = true;
                internal_error = Some(e);
            }
        }
        if let Some(e) = internal_error {
            return Err(e);
        }
        Ok(())
    }); // all scoped workers join, including on early error, before releasing lock
    result?;
    summary(&l, Some(&state), &root)
}
pub fn execute(command: ProjectCommand) -> Result<Value> {
    match command {
        ProjectCommand::Schema => Ok(schema()),
        ProjectCommand::Init { manifest } => {
            let value = example();
            crate::io::write_json(&manifest, &value, false)?;
            Ok(json!({"created":manifest,"paid_request":false,"manifest":value}))
        }
        ProjectCommand::Validate {
            manifest,
            check_models,
            cache_dir,
            offline,
        } => {
            let l = load(&manifest)?;
            if check_models {
                let state = initial(&l)?;
                preflight(
                    &l,
                    &state,
                    &RunArgs {
                        manifest,
                        concurrency: 2,
                        keep_going: false,
                        cache_dir,
                        offline,
                        wait: 0,
                    },
                )?;
            }
            Ok(
                json!({"valid":true,"version":1,"nodes":l.manifest.nodes.len(),"model_capabilities_checked":check_models,"paid_request":false,"warnings":warnings(&l)}),
            )
        }
        ProjectCommand::Run(args) => run(args, false, Engine::default()),
        ProjectCommand::Resume(args) => run(args, true, Engine::default()),
        ProjectCommand::Status { manifest } => {
            let l = load(&manifest)?;
            let root = state_dir(&l)?;
            let _lock = lock(&root)?;
            let state = read_state(&l, &root)?;
            if let Some(s) = &state {
                for n in &l.manifest.nodes {
                    if s.nodes[&n.id].status == "completed"
                        && crate::media::file_sha256(&output(&l, n)?)?
                            != s.nodes[&n.id]
                                .output_sha256
                                .as_deref()
                                .context("missing output hash")?
                    {
                        bail!("completed output changed for {}", n.id);
                    }
                }
            }
            summary(&l, state.as_ref(), &root)
        }
    }
}
pub fn example() -> Value {
    json!({"version":1,"output_root":"generated","nodes":[
        {"id":"character","output":"character.png","model":null,"action":{"type":"image","prompt":"A consistent illustrated cloud character, plain background."}},
        {"id":"opening","output":"opening.mp4","model":null,"shot":{"size":"wide","angle":"eye level","motion":"gentle push in"},"action":{"type":"video","prompt":"The cloud character enters a sunlit garden.","first_frame":{"node":"character"},"duration":6}},
        {"id":"narration","output":"narration.mp3","model":null,"action":{"type":"speech","text":"Every small cloud has a story.","voice":"af_heart"}},
        {"id":"score","output":"score.mp3","model":null,"action":{"type":"music","prompt":"Gentle whimsical instrumental storybook score.","instrumental":true}}
    ]})
}
pub fn schema() -> Value {
    // Portable JSON Schema; manifest parsing independently denies unknown fields.
    json!({"$schema":"https://json-schema.org/draft/2020-12/schema","title":"Skywrite generation project v1","type":"object","additionalProperties":false,"required":["version","output_root","nodes"],"properties":{"version":{"const":1},"output_root":{"type":"string","description":"Manifest-relative output directory, no dot/parent components or symlinks"},"nodes":{"type":"array","minItems":1,"maxItems":MAX_NODES,"items":{"$ref":"#/$defs/node"}}},"$defs":{
        "reference":{"oneOf":[{"type":"object","additionalProperties":false,"required":["file"],"properties":{"file":{"type":"string"}}},{"type":"object","additionalProperties":false,"required":["node"],"properties":{"node":{"type":"string"}}}]},
        "shot":{"type":"object","additionalProperties":false,"required":["size","angle","motion"],"properties":{"size":{"type":"string","maxLength":128},"angle":{"type":"string","maxLength":128},"motion":{"type":"string","maxLength":128}}},
        "node":{"type":"object","additionalProperties":false,"required":["id","output","action"],"properties":{"id":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,64}$"},"output":{"type":"string"},"depends_on":{"type":"array","maxItems":MAX_NODES,"items":{"type":"string"}},"model":{"type":["string","null"]},"take":{"type":["string","null"]},"shot":{"anyOf":[{"$ref":"#/$defs/shot"},{"type":"null"}]},"action":{"oneOf":[{"$ref":"#/$defs/image"},{"$ref":"#/$defs/video"},{"$ref":"#/$defs/speech"},{"$ref":"#/$defs/music"}]}}},
        "image":action_schema("image",json!({"prompt":{"type":"string"},"references":{"type":"array","maxItems":10,"items":{"$ref":"#/$defs/reference"}},"resolution":{"type":["string","null"]},"aspect_ratio":{"type":["string","null"]},"seed":{"type":["integer","null"],"minimum":0}}),"prompt"),
        "video":action_schema("video",json!({"prompt":{"type":"string"},"references":{"type":"array","maxItems":10,"items":{"$ref":"#/$defs/reference"}},"first_frame":{"anyOf":[{"$ref":"#/$defs/reference"},{"type":"null"}]},"last_frame":{"anyOf":[{"$ref":"#/$defs/reference"},{"type":"null"}]},"duration":{"type":["integer","null"],"minimum":0},"resolution":{"type":["string","null"]},"aspect_ratio":{"type":["string","null"]},"seed":{"type":["integer","null"],"minimum":0},"audio":{"type":["boolean","null"]}}),"prompt"),
        "speech":action_schema("speech",json!({"text":{"type":"string"},"voice":{"type":["string","null"]},"instructions":{"type":["string","null"]},"speed":{"type":["number","null"],"minimum":0.5,"maximum":2.0}}),"text"),
        "music":action_schema("music",json!({"prompt":{"type":"string"},"instrumental":{"type":"boolean"},"reference":{"anyOf":[{"$ref":"#/$defs/reference"},{"type":"null"}]}}),"prompt")
    },"examples":[example()]})
}
fn action_schema(kind: &str, mut properties: Value, required: &str) -> Value {
    properties["type"] = json!({"const":kind});
    json!({"type":"object","additionalProperties":false,"required":["type",required],"properties":properties})
}

#[cfg(test)]
mod tests;
