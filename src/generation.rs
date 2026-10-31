//! Explicit generation with durable intent, resumable provider jobs, and provenance.
use anyhow::{Context, Result, bail};
use base64::Engine;
use clap::{Args, Subcommand};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
const BASE: &str = "https://openrouter.ai/api/v1";
const MAX_REQUEST: usize = 64 * 1024 * 1024;
const MAX_JSON: usize = 64 * 1024 * 1024;
const MAX_IMAGE: usize = 32 * 1024 * 1024;
const MAX_VIDEO: u64 = 512 * 1024 * 1024;
const MAX_PIXELS: u64 = 64 * 1024 * 1024;
#[derive(Debug, Args)]
pub struct Common {
    #[arg(long)]
    pub model: Option<String>,
    /// Optional publication path; original result remains in the job folder.
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub overwrite: bool,
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
    /// Use cached model metadata; generation itself still uses the network.
    #[arg(long)]
    pub offline: bool,
    /// Validate capabilities without making a paid request.
    #[arg(long)]
    pub dry_run: bool,
    /// Intentional variation identity. Reusing the same take resumes its job.
    #[arg(long)]
    pub take: Option<String>,
}
#[derive(Debug, Args)]
pub struct ImageArgs {
    #[arg(long)]
    pub prompt: String,
    #[arg(long)]
    pub reference: Vec<PathBuf>,
    #[arg(long)]
    pub resolution: Option<String>,
    #[arg(long)]
    pub aspect_ratio: Option<String>,
    #[arg(long)]
    pub seed: Option<u64>,
    #[command(flatten)]
    pub common: Common,
}
#[derive(Debug, Args)]
pub struct VideoArgs {
    #[arg(long)]
    pub prompt: String,
    #[arg(long)]
    pub reference: Vec<PathBuf>,
    #[arg(long, conflicts_with = "reference")]
    pub first_frame: Option<PathBuf>,
    #[arg(long, conflicts_with = "reference")]
    pub last_frame: Option<PathBuf>,
    #[arg(long)]
    pub duration: Option<u32>,
    #[arg(long)]
    pub resolution: Option<String>,
    #[arg(long)]
    pub aspect_ratio: Option<String>,
    #[arg(long)]
    pub seed: Option<u64>,
    #[arg(long)]
    pub audio: Option<bool>,
    /// Wait up to this many seconds. Zero returns a resumable job immediately.
    #[arg(long, default_value_t = 0)]
    pub wait: u64,
    #[command(flatten)]
    pub common: Common,
}
#[derive(Debug, Subcommand)]
pub enum GenerateCommand {
    Image(ImageArgs),
    Video(VideoArgs),
    /// Generate narrated speech through OpenRouter.
    Speech(crate::audio_generation::SpeechArgs),
    /// Generate original music through an audio-output model.
    Music(crate::audio_generation::MusicArgs),
}
#[derive(Debug, Args)]
pub struct VideoUpscaleArgs {
    /// Optional local file for integrity/provenance. OpenRouter needs an HTTPS source.
    #[arg(required_unless_present_any=["source_job","source_url"])]
    pub input: Option<PathBuf>,
    /// Completed Skywrite video job; obtains a fresh signed OpenRouter content URL.
    #[arg(long, conflicts_with = "source_url")]
    pub source_job: Option<String>,
    /// Public HTTPS source you host; alternatively opt into the configured S3/R2 uploader.
    #[arg(long, conflicts_with = "source_job")]
    pub source_url: Option<String>,
    /// Upload/reuse the local MP4 in the explicitly configured public S3/R2 bucket.
    #[arg(long, conflicts_with_all=["source_job", "source_url"])]
    pub upload: bool,
    /// Remove the uploaded source after a verified completed result; resume retries cleanup.
    #[arg(long, requires = "upload")]
    pub delete_upload_after: bool,
    #[arg(long, default_value_t = 2.0)]
    pub factor: f64,
    /// 0 precise, 1 creative. Creative processing may invent details.
    #[arg(long, default_value_t = 0)]
    pub creativity: u8,
    #[arg(long)]
    pub prompt: Option<String>,
    #[arg(long, default_value_t = 0)]
    pub wait: u64,
    #[command(flatten)]
    pub common: Common,
}
#[derive(Debug, Subcommand)]
pub enum UpscaleCommand {
    /// Local FFmpeg Lanczos resizing, without AI or invented detail.
    Image {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long, default_value_t = 2.0)]
        factor: f64,
        #[arg(long)]
        overwrite: bool,
    },
    /// Dedicated OpenRouter video upscaling with resumable jobs.
    Video(VideoUpscaleArgs),
}
#[derive(Debug, Subcommand)]
pub enum JobsCommand {
    List,
    /// Read local state; --refresh polls the known job without resubmitting.
    Status {
        id: String,
        #[arg(long)]
        refresh: bool,
    },
    /// Retrieve/wait for an existing job. Never submits a paid generation.
    Resume {
        id: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[arg(long)]
        overwrite: bool,
        #[arg(long, default_value_t = 0)]
        wait: u64,
        /// Adopt a known provider video ID after an ambiguous submit outcome.
        #[arg(long)]
        remote_id: Option<String>,
    },
}
#[derive(Serialize, Deserialize, Debug, Clone)]
struct Artifact {
    file: String,
    sha256: String,
    media_type: String,
    bytes: u64,
    /// Set only after this version actually decodes these hash-identified bytes.
    #[serde(default)]
    decode_validation_version: u32,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
struct Job {
    version: u32,
    id: String,
    task: String,
    model: String,
    status: String,
    created_at: u64,
    updated_at: u64,
    request: Value,
    references: Vec<Value>,
    remote_id: Option<String>,
    generation_id: Option<String>,
    usage: Option<Value>,
    artifact: Option<Artifact>,
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_text: Option<String>,
}
fn timestamp() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
fn valid_id(id: &str) -> Result<()> {
    if id.len() != 64 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("job ID must be its 64-character request hash");
    }
    Ok(())
}
fn remote_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        bail!("invalid provider video job ID");
    }
    Ok(())
}
fn jobs_root() -> Result<PathBuf> {
    Ok(crate::workspace::personal_home()?.join("jobs"))
}
// Unknown job state must never be mistaken for absence. Inspect the entry
// itself so dangling/special records also reach the validating job reader.
fn job_path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("inspecting job state {}", path.display()))
        }
    }
}
fn open_regular(path: &Path, create: bool) -> Result<File> {
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
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("job inputs must be regular non-symlink files");
    }
    Ok(file)
}
fn limited(reader: impl Read, max: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(max as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max {
        bail!("generation input/response exceeds {max} bytes");
    }
    Ok(bytes)
}
fn locked_job(id: &str) -> Result<(PathBuf, File)> {
    valid_id(id)?;
    let dir = jobs_root()?.join(id);
    fs::create_dir_all(&dir)?;
    if !fs::symlink_metadata(&dir)?.is_dir() {
        bail!("job directory must not be a symlink");
    }
    // Make the newly created directory chain durable before recording intent.
    #[cfg(unix)]
    for ancestor in dir.canonicalize()?.ancestors() {
        File::open(ancestor)?.sync_all()?;
    }
    let lock = open_regular(&dir.join(".lock"), true)?;
    lock.lock().context("locking generation job")?;
    Ok((dir, lock))
}
fn load_job(dir: &Path) -> Result<Job> {
    let job: Job = serde_json::from_slice(&limited(
        open_regular(&dir.join("job.json"), false)?,
        1024 * 1024,
    )?)?;
    if job.version != 1 {
        bail!("unsupported generation job version");
    }
    valid_id(&job.id)?;
    if dir.file_name().and_then(|s| s.to_str()) != Some(job.id.as_str()) {
        bail!("job identity does not match its folder");
    }
    if !matches!(
        job.task.as_str(),
        "image-generation"
            | "video-generation"
            | "video-upscaling"
            | "speech-generation"
            | "music-generation"
    ) {
        bail!("invalid generation task");
    }
    if let Some(id) = &job.remote_id {
        remote_id(id)?;
    }
    if !matches!(
        job.status.as_str(),
        "prepared"
            | "submitting"
            | "uncertain"
            | "rejected"
            | "received"
            | "pending"
            | "in_progress"
            | "ready"
            | "failed"
            | "completed"
    ) {
        bail!("invalid persisted generation state");
    }
    if job.status == "completed" && job.artifact.is_none() {
        bail!("completed job lacks artifact metadata");
    }
    if matches!(job.status.as_str(), "pending" | "in_progress" | "ready") && job.remote_id.is_none()
    {
        bail!("active video job lacks provider ID");
    }
    Ok(job)
}
fn save_job(dir: &Path, job: &mut Job) -> Result<()> {
    job.updated_at = timestamp()?;
    let bytes = serde_json::to_vec_pretty(job)?;
    if bytes.len() > 1024 * 1024 {
        bail!("job metadata exceeds one MiB");
    }
    crate::io::atomic_write(&dir.join("job.json"), &bytes, true)?;
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}
fn artifact_path(dir: &Path, a: &Artifact) -> Result<PathBuf> {
    if !matches!(
        a.file.as_str(),
        "result.png" | "result.jpg" | "result.webp" | "result.mp4" | "result.mp3" | "result.wav"
    ) {
        bail!("invalid job artifact name");
    }
    let expected_mime = match a.file.as_str() {
        "result.png" => "image/png",
        "result.jpg" => "image/jpeg",
        "result.webp" => "image/webp",
        "result.mp4" => "video/mp4",
        "result.mp3" => "audio/mpeg",
        "result.wav" => "audio/wav",
        _ => unreachable!(),
    };
    if a.media_type != expected_mime {
        bail!("job artifact filename disagrees with MIME metadata");
    }
    let max_bytes = match a.media_type.as_str() {
        "video/mp4" => MAX_VIDEO,
        "audio/mpeg" | "audio/wav" => crate::audio_generation::MAX_AUDIO as u64,
        _ => MAX_IMAGE as u64,
    };
    if a.bytes > max_bytes {
        bail!("job artifact exceeds its media byte limit");
    }
    let path = dir.join(&a.file);
    let f = open_regular(&path, false)?;
    if f.metadata()?.len() != a.bytes
        || crate::media::file_sha256_limited(&path, a.bytes)? != a.sha256
    {
        bail!("job artifact integrity check failed");
    }
    Ok(path)
}

/// Actual consumers verify legacy artifacts, while read-only job metadata does
/// not repeatedly decode completed videos. The hash binds the decode marker to
/// unchanged bytes in this user-owned job store.
fn verified_artifact_path(dir: &Path, artifact: &Artifact) -> Result<PathBuf> {
    let path = artifact_path(dir, artifact)?;
    if artifact.decode_validation_version != 1 {
        if is_audio_task_mime(&artifact.media_type) {
            crate::audio_generation::validate_audio(&path, &artifact.media_type)?;
        } else {
            validate_generated_media(&path, &artifact.media_type)?;
        }
    }
    Ok(path)
}

/// Decode the actual primary media, rather than trusting container headers.
/// This is also used for cached artifacts, so a result accepted by an older
/// version cannot bypass validation through a same-format copy.
fn validate_generated_media(path: &Path, media_type: &str) -> Result<()> {
    let video = media_type == "video/mp4";
    if video {
        if open_regular(path, false)?.metadata()?.len() > MAX_VIDEO {
            bail!("video result exceeds 512 MiB");
        }
        let media = crate::media::probe_for_timing(path)?;
        if !media.has_video()
            || !media
                .format_name
                .split(',')
                .any(|s| matches!(s, "mov" | "mp4"))
            || media.video().is_none_or(|stream| {
                stream.width.unwrap_or(0) == 0
                    || stream.height.unwrap_or(0) == 0
                    || u64::from(stream.width.unwrap_or(0)) * u64::from(stream.height.unwrap_or(0))
                        > MAX_PIXELS
            })
        {
            bail!("video result must be MP4 with valid dimensions up to 64 megapixels");
        }
    } else {
        if !matches!(media_type, "image/png" | "image/jpeg" | "image/webp") {
            bail!("unsupported generated artifact MIME type");
        }
        let bytes = limited(open_regular(path, false)?, MAX_IMAGE)?;
        let (actual, _) = image_type(&bytes)?;
        let dimensions = imagesize::blob_size(&bytes)?;
        if actual != media_type
            || dimensions.width == 0
            || dimensions.height == 0
            || (dimensions.width as u64).saturating_mul(dimensions.height as u64) > MAX_PIXELS
        {
            bail!("image result MIME or dimensions are invalid");
        }
    }
    let mut args = crate::process::args(&[
        "-v",
        "error",
        "-nostdin",
        "-xerror",
        "-err_detect",
        "explode",
        "-threads",
        "1",
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]);
    args.push(path.as_os_str().to_owned());
    args.extend(crate::process::args(&["-map", "0:v:0"]));
    if video {
        args.extend(crate::process::args(&["-map", "0:a:0?"]));
    } else {
        args.extend(crate::process::args(&["-frames:v", "1", "-an"]));
    }
    args.extend(crate::process::args(&[
        "-progress",
        "pipe:1",
        "-nostats",
        "-f",
        "null",
        "-",
    ]));
    let decoded = crate::process::run(
        "ffmpeg",
        &args,
        Duration::from_secs(if video { 180 } else { 30 }),
        1024 * 1024,
    )
    .context("generated media failed decode validation")?;
    if !String::from_utf8_lossy(&decoded.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("frame="))
        .any(|value| value.trim().parse::<u64>().is_ok_and(|frames| frames > 0))
    {
        bail!("generated media failed decode validation: no video frame decoded");
    }
    Ok(())
}
fn public_job(dir: &Path, job: &Job) -> Result<Value> {
    let mut out = serde_json::to_value(job)?;
    out["job_file"] = json!(dir.join("job.json"));
    if let Some(a) = &job.artifact {
        out["artifact_path"] = json!(artifact_path(dir, a)?);
    }
    out["resume"] = json!(format!("skywrite jobs resume {}", job.id));
    Ok(out)
}
#[derive(Debug)]
struct HttpRejection {
    status: u16,
    message: String,
}
impl std::fmt::Display for HttpRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OpenRouter HTTP {}: {}", self.status, self.message)
    }
}
impl std::error::Error for HttpRejection {}
struct Transport {
    client: Client,
    key: String,
    base: String,
}
#[cfg(test)]
thread_local! { static TEST_TRANSPORT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
pub(crate) fn with_test_transport<T>(base: &str, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_TRANSPORT.with(|cell| {
                *cell.borrow_mut() = self.0.take();
            });
        }
    }
    let old = TEST_TRANSPORT.with(|cell| cell.replace(Some(base.into())));
    let _restore = Restore(old);
    work()
}
impl Transport {
    fn new() -> Result<Self> {
        #[cfg(test)]
        if let Some(base) = TEST_TRANSPORT.with(|cell| cell.borrow().clone()) {
            let parsed = url::Url::parse(&base)?;
            if parsed.scheme() != "http" || parsed.host_str() != Some("127.0.0.1") {
                bail!("test transport must be owned loopback HTTP");
            }
            return Ok(Self {
                client: Self::client()?,
                key: "owned-test-fixture".into(),
                base,
            });
        }
        let key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .context("set OPENROUTER_API_KEY for generation/job retrieval")?;
        Ok(Self {
            client: Self::client()?,
            key,
            base: BASE.into(),
        })
    }
    fn client() -> Result<Client> {
        Self::client_with_deadline(Duration::from_secs(300))
    }
    fn client_with_deadline(deadline: Duration) -> Result<Client> {
        Ok(Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(deadline)
            .redirect(reqwest::redirect::Policy::none())
            .build()?)
    }
    fn diagnostic(&self, raw: &str, body: Option<&Value>) -> String {
        let mut message = raw.to_owned();
        if let Some(body) = body {
            let mut urls = Vec::new();
            collect_reference_urls(body, &mut urls);
            for url in urls {
                if !url.is_empty() {
                    message = message.replace(url, "<redacted reference URL>");
                }
            }
        }
        message
            .replace(&self.key, "<redacted API key>")
            .chars()
            .take(2048)
            .collect()
    }
    fn json(&self, path: &str, body: Option<&Value>) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        let request = if let Some(body) = body {
            let encoded = serde_json::to_vec(body)?;
            if encoded.len() > MAX_REQUEST {
                bail!("generation request exceeds 64 MiB");
            }
            self.client
                .post(url)
                .header("Content-Type", "application/json")
                .body(encoded)
        } else {
            self.client.get(url)
        };
        let response = request
            .bearer_auth(&self.key)
            .header("X-Title", "Skywrite")
            .send()?;
        let status = response.status();
        let bytes = limited(response, MAX_JSON)?;
        if !status.is_success() {
            return Err(HttpRejection {
                status: status.as_u16(),
                message: self.diagnostic(&String::from_utf8_lossy(&bytes), body),
            }
            .into());
        }
        let value: Value = serde_json::from_slice(&bytes)
            .context("OpenRouter generation response was not JSON")?;
        if !value["error"].is_null() {
            bail!(
                "OpenRouter generation error: {}",
                self.diagnostic(&value["error"].to_string(), body)
            );
        }
        Ok(value)
    }
    fn content(&self, id: &str, destination: &Path) -> Result<()> {
        remote_id(id)?;
        let mut url = url::Url::parse(&format!("{}/videos/{id}/content?index=0", self.base))?;
        let origin = url.origin();
        for hop in 0..5 {
            let response = if hop == 0 {
                self.client.get(url.clone()).bearer_auth(&self.key).send()?
            } else {
                if cfg!(test) && url.origin() == origin {
                    self.client.get(url.clone()).send()?
                } else {
                    public_download_client(&url)?.get(url.clone()).send()?
                }
            };
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .context("video redirect lacks Location")?
                    .to_str()?;
                url = url.join(location)?;
                if url.origin() != origin || !cfg!(test) {
                    validate_public_url(&url)?;
                }
                continue;
            }
            if !response.status().is_success() {
                bail!("video content download HTTP {}", response.status());
            }
            if response.content_length().is_some_and(|n| n > MAX_VIDEO) {
                bail!("video output exceeds 512 MiB");
            }
            let mut output = File::create(destination)?;
            let count = std::io::copy(&mut response.take(MAX_VIDEO + 1), &mut output)?;
            if count > MAX_VIDEO {
                bail!("video output exceeds 512 MiB");
            }
            output.sync_all()?;
            return Ok(());
        }
        bail!("too many video content redirects")
    }
}
pub(crate) fn validate_public_url(url: &url::Url) -> Result<()> {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || url.fragment().is_some()
    {
        bail!("video downloads require public HTTPS URLs without userinfo");
    }
    let host = url.host_str().context("video content URL lacks host")?;
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        bail!("private video download host rejected");
    }
    Ok(())
}
// Conservative subset of globally reachable unicast. See the checked-in IANA
// registries (retrieved 2026-10-30) and endpoint table under tests/fixtures/network.
// Entire special-use parent ranges may be denied even when they contain globally
// reachable exceptions; mapped/NAT64/6to4/Teredo addresses remain denied.
fn public_address(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let o = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_unspecified()
                || o[0] == 0
                || o[0] >= 240
                || o[0] == 127
                || o[0] == 100 && (64..=127).contains(&o[1])
                || o[0] == 192 && (o[1] == 0 || o[1] == 88 && o[2] == 99)
                || o[0] == 198 && (o[1] == 18 || o[1] == 19)
                || o[0] == 198 && o[1] == 51 && o[2] == 100
                || o[0] == 203 && o[1] == 0 && o[2] == 113)
        }
        std::net::IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
        }
    }
}
pub(crate) fn public_download_client(url: &url::Url) -> Result<Client> {
    use std::net::ToSocketAddrs;
    validate_public_url(url)?;
    let host = url.host().context("missing content host")?;
    // URL's typed host avoids treating brackets around an IPv6 literal as DNS.
    let addresses = match host {
        url::Host::Ipv4(ip) => vec![std::net::SocketAddr::new(ip.into(), 443)],
        url::Host::Ipv6(ip) => vec![std::net::SocketAddr::new(ip.into(), 443)],
        url::Host::Domain(name) => (name, 443).to_socket_addrs()?.collect::<Vec<_>>(),
    };
    if addresses.is_empty() || addresses.iter().any(|a| !public_address(a.ip())) {
        bail!("nonpublic video download address rejected");
    }
    Ok(pinned_download_builder(&host.to_string(), &addresses).build()?)
}
// Production calls this only after the complete resolved set passes admission.
// Tests exercise this same socket/TLS boundary with owned loopback addresses and
// a test root certificate, independently of the pure global-address guard.
fn pinned_download_builder(
    host: &str,
    addresses: &[std::net::SocketAddr],
) -> reqwest::blocking::ClientBuilder {
    Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .resolve_to_addrs(host, addresses)
}

#[cfg(test)]
pub(crate) fn public_test_client(
    host: &str,
    addresses: &[std::net::SocketAddr],
) -> reqwest::blocking::ClientBuilder {
    pinned_download_builder(host, addresses)
}

fn prompt_check(prompt: &str) -> Result<()> {
    if prompt.trim().is_empty() || prompt.len() > 32 * 1024 {
        bail!("prompt must contain 1..32768 bytes");
    }
    Ok(())
}
fn image_type(bytes: &[u8]) -> Result<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok(("image/png", "png"))
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok(("image/jpeg", "jpg"))
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Ok(("image/webp", "webp"))
    } else {
        bail!("only real PNG/JPEG/WebP raster images are supported");
    }
}
fn image_bytes(path: &Path) -> Result<(Vec<u8>, String)> {
    let bytes = limited(open_regular(path, false)?, 16 * 1024 * 1024)?;
    let (mime, _) = image_type(&bytes)?;
    let size = imagesize::blob_size(&bytes)?;
    if size.width == 0
        || size.height == 0
        || (size.width as u64).saturating_mul(size.height as u64) > MAX_PIXELS
    {
        bail!("image reference exceeds 64 megapixels");
    }
    Ok((bytes, mime.into()))
}
pub(crate) fn bytes_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn add_image(path: &Path, refs: &mut Vec<Value>) -> Result<Value> {
    let (bytes, mime) = image_bytes(path)?;
    let retained: u64 = refs.iter().filter_map(|r| r["bytes"].as_u64()).sum();
    if retained.saturating_add(bytes.len() as u64) > 32 * 1024 * 1024 {
        bail!("combined image references exceed 32 MiB");
    }
    let hash = bytes_sha256(&bytes);
    refs.push(
        json!({"path":path.canonicalize()?,"sha256":hash,"media_type":mime,"bytes":bytes.len()}),
    );
    Ok(
        json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{}",base64::engine::general_purpose::STANDARD.encode(bytes))}}),
    )
}
fn image_parameter(model: &Value, body: &mut Value, key: &str, value: Option<Value>) -> Result<()> {
    let Some(value) = value else { return Ok(()) };
    let cap = &model["supported_parameters"][key];
    if cap.is_null() {
        bail!("model does not advertise {key} support");
    }
    match cap["type"].as_str() {
        Some("enum") if !cap["values"].as_array().is_some_and(|v| v.contains(&value)) => {
            bail!("unsupported {key}; model accepts {}", cap["values"])
        }
        Some("range") => {
            let n = value.as_f64().context("numeric model parameter required")?;
            if n < cap["min"].as_f64().unwrap_or(f64::NEG_INFINITY)
                || n > cap["max"].as_f64().unwrap_or(f64::INFINITY)
            {
                bail!("{key} is outside model capability range");
            }
        }
        _ => {}
    }
    body[key] = value;
    Ok(())
}
fn video_enum(
    model: &Value,
    body: &mut Value,
    key: &str,
    field: &str,
    value: Option<Value>,
) -> Result<()> {
    let Some(value) = value else { return Ok(()) };
    if !model[field].as_array().is_some_and(|v| v.contains(&value)) {
        bail!("unsupported {key}; model advertises {}", model[field]);
    }
    body[key] = value;
    Ok(())
}
fn collect_reference_urls<'a>(value: &'a Value, urls: &mut Vec<&'a str>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if matches!(key.as_str(), "image_url" | "video_url")
                    && let Some(url) = value["url"].as_str()
                {
                    urls.push(url);
                }
                collect_reference_urls(value, urls);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_reference_urls(value, urls);
            }
        }
        _ => {}
    }
}
fn sanitized_request(body: &Value) -> Value {
    fn hide(value: &mut Value) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    if matches!(key.as_str(), "image_url" | "video_url") && value["url"].is_string()
                    {
                        value["url"] = json!("<content-addressed local reference>");
                    } else {
                        hide(value);
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    hide(value);
                }
            }
            _ => {}
        }
    }
    let mut value = body.clone();
    hide(&mut value);
    value
}

struct Prepared {
    task: String,
    model: String,
    body: Value,
    references: Vec<Value>,
    common: Common,
    wait: u64,
}
thread_local! {
    static EXPECTED_PROJECT_JOB: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}
/// Bind a project's durable admission to the exact request assembled by adapters.
/// Rechecking at submit closes input/model changes between planning and execution.
pub(crate) fn execute_generate_planned(command: GenerateCommand, expected: &str) -> Result<Value> {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            EXPECTED_PROJECT_JOB.with(|cell| {
                *cell.borrow_mut() = self.0.take();
            });
        }
    }
    let old = EXPECTED_PROJECT_JOB.with(|cell| cell.replace(Some(expected.into())));
    let _restore = Restore(old);
    execute_generate(command)
}
pub fn execute_generate(command: GenerateCommand) -> Result<Value> {
    let prepared = match command {
        GenerateCommand::Speech(args) => return crate::audio_generation::speech(args),
        GenerateCommand::Music(args) => return crate::audio_generation::music(args),
        GenerateCommand::Image(args) => {
            prompt_check(&args.prompt)?;
            let model =
                crate::models::resolve_model("image-generation", args.common.model.as_deref())?;
            let caps = crate::models::find_model(
                "image-generation",
                &model,
                args.common.cache_dir.as_deref(),
                args.common.offline,
            )?;
            let mut body = json!({"model":model,"prompt":args.prompt,"n":1});
            let mut references = Vec::new();
            image_parameter(
                &caps,
                &mut body,
                "resolution",
                args.resolution.map(Value::String),
            )?;
            image_parameter(
                &caps,
                &mut body,
                "aspect_ratio",
                args.aspect_ratio.map(Value::String),
            )?;
            image_parameter(&caps, &mut body, "seed", args.seed.map(|s| json!(s)))?;
            if !args.reference.is_empty() {
                let cap = &caps["supported_parameters"]["input_references"];
                let max = cap["max"].as_u64().unwrap_or(0);
                if args.reference.len() as u64 > max
                    || (args.reference.len() as u64) < cap["min"].as_u64().unwrap_or(0)
                {
                    bail!("image reference count is outside model capability range");
                }
                body["input_references"] = Value::Array(
                    args.reference
                        .iter()
                        .map(|p| add_image(p, &mut references))
                        .collect::<Result<_>>()?,
                );
            } else if caps["supported_parameters"]["input_references"]["min"]
                .as_u64()
                .unwrap_or(0)
                > 0
            {
                bail!("this image model requires references");
            }
            Prepared {
                task: "image-generation".into(),
                model,
                body,
                references,
                common: args.common,
                wait: 0,
            }
        }
        GenerateCommand::Video(args) => {
            prompt_check(&args.prompt)?;
            wait_check(args.wait)?;
            let model =
                crate::models::resolve_model("video-generation", args.common.model.as_deref())?;
            let caps = crate::models::find_model(
                "video-generation",
                &model,
                args.common.cache_dir.as_deref(),
                args.common.offline,
            )?;
            let mut body = json!({"model":model,"prompt":args.prompt});
            let mut references = Vec::new();
            video_enum(
                &caps,
                &mut body,
                "duration",
                "supported_durations",
                args.duration.map(|v| json!(v)),
            )?;
            video_enum(
                &caps,
                &mut body,
                "resolution",
                "supported_resolutions",
                args.resolution.map(Value::String),
            )?;
            video_enum(
                &caps,
                &mut body,
                "aspect_ratio",
                "supported_aspect_ratios",
                args.aspect_ratio.map(Value::String),
            )?;
            if let Some(seed) = args.seed {
                if caps["seed"] != true {
                    bail!("model does not advertise seed support");
                }
                body["seed"] = json!(seed);
            }
            if let Some(audio) = args.audio {
                if audio && caps["generate_audio"] != true {
                    bail!("model does not advertise audio generation");
                }
                body["generate_audio"] = json!(audio);
            }
            let mut frames = Vec::new();
            for (kind, path) in [
                ("first_frame", args.first_frame),
                ("last_frame", args.last_frame),
            ] {
                if let Some(path) = path {
                    if !caps["supported_frame_images"]
                        .as_array()
                        .is_some_and(|a| a.contains(&json!(kind)))
                    {
                        bail!("model does not advertise {kind} support");
                    }
                    let mut frame = add_image(&path, &mut references)?;
                    frame["frame_type"] = json!(kind);
                    frames.push(frame);
                }
            }
            if model == "minimax/hailuo-3-max" && frames.len() > 1 {
                bail!(
                    "H3 Max supports one keyframe per request: choose --first-frame OR --last-frame"
                );
            }
            if !frames.is_empty() {
                body["frame_images"] = json!(frames);
            }
            if !args.reference.is_empty() {
                if !caps["architecture"]["input_modalities"]
                    .as_array()
                    .is_some_and(|v| v.contains(&json!("image")))
                {
                    bail!("model does not advertise image reference input");
                }
                if args.reference.len() > 10 {
                    bail!("at most ten video reference images are supported");
                }
                body["input_references"] = Value::Array(
                    args.reference
                        .iter()
                        .map(|p| add_image(p, &mut references))
                        .collect::<Result<_>>()?,
                );
            }
            Prepared {
                task: "video-generation".into(),
                model,
                body,
                references,
                common: args.common,
                wait: args.wait,
            }
        }
    };
    submit(prepared)
}
fn wait_check(wait: u64) -> Result<()> {
    if wait > 1800 {
        bail!("wait must be at most 1800 seconds");
    }
    Ok(())
}
fn validate_output(output: Option<&Path>, inputs: &[Value], overwrite: bool) -> Result<()> {
    if let Some(path) = output {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut parent = absolute.parent().context("output lacks parent")?;
        while !parent.exists() {
            parent = parent.parent().context("output lacks existing ancestor")?;
        }
        let root = jobs_root()?;
        if root.exists() && parent.canonicalize()?.starts_with(root.canonicalize()?) {
            bail!(
                "publication cannot overwrite internal generation job files; choose another output folder"
            );
        }
        if path.exists() {
            let actual = path.canonicalize()?;
            if inputs
                .iter()
                .any(|v| v["path"].as_str().is_some_and(|p| Path::new(p) == actual))
            {
                bail!("output would overwrite a reference input");
            }
            if !overwrite {
                bail!("output exists; use --overwrite to replace it");
            }
        }
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !["png", "jpg", "jpeg", "webp", "mp4", "mp3", "wav"].contains(&extension.as_str()) {
            bail!("output must have PNG/JPEG/WebP/MP4/MP3/WAV extension");
        }
    }
    Ok(())
}
fn submit(prepared: Prepared) -> Result<Value> {
    submit_with(prepared, None)
}
fn submit_with(prepared: Prepared, transport: Option<&Transport>) -> Result<Value> {
    submit_with_upload(prepared, transport, None)
}
fn submit_with_upload(
    prepared: Prepared,
    transport: Option<&Transport>,
    upload: Option<crate::storage::UploadPlan>,
) -> Result<Value> {
    let Prepared {
        task,
        model,
        body,
        references,
        common,
        wait,
    } = prepared;
    if let Some(take) = &common.take
        && (take.is_empty() || take.len() > 128 || take.chars().any(char::is_control))
    {
        bail!("take identity must be 1..128 printable bytes");
    }
    validate_output(common.output.as_deref(), &references, common.overwrite)?;
    if let Some(output) = &common.output {
        let ext = output
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let compatible = match task.as_str() {
            "image-generation" => matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp"),
            "speech-generation" | "music-generation" => matches!(ext.as_str(), "mp3" | "wav"),
            _ => ext == "mp4",
        };
        if !compatible {
            bail!("output extension does not match requested media kind");
        }
    }

    let mut identity_body = body.clone();
    if task == "video-upscaling" {
        identity_body["input_references"][0]["video_url"]["url"] = references
            .first()
            .and_then(|r| r.get("sha256"))
            .cloned()
            .context("upscale source lacks stable identity")?;
    }
    let encoded = serde_json::to_vec(&body)?;
    if encoded.len() > MAX_REQUEST {
        bail!("generation request exceeds 64 MiB");
    }
    let id = crate::openrouter::cache_key(&[
        b"skywrite-generation-v1",
        task.as_bytes(),
        &serde_json::to_vec(&identity_body)?,
        common.take.as_deref().unwrap_or("").as_bytes(),
    ]);
    EXPECTED_PROJECT_JOB.with(|cell| -> Result<()> {
        if cell
            .borrow()
            .as_ref()
            .is_some_and(|expected| expected != &id)
        {
            bail!("project planned identity changed before generation; no request submitted");
        }
        Ok(())
    })?;
    let request = sanitized_request(&body);
    if common.dry_run {
        return Ok(
            json!({"dry_run":true,"id":id,"task":task,"model":model,"request":request,"references":references,"paid_request":false,"note":"Pricing comes from selected model catalog/endpoint; this preview is not a quote."}),
        );
    }
    let requested_cleanup = references
        .first()
        .and_then(|r| r["cleanup_requested"].as_bool())
        .unwrap_or(false);
    let (dir, _lock) = locked_job(&id)?;
    let mut job = if job_path_exists(&dir.join("job.json"))? {
        load_job(&dir)?
    } else {
        let now = timestamp()?;
        let mut job = Job {
            version: 1,
            id,
            task,
            model,
            status: "prepared".into(),
            created_at: now,
            updated_at: now,
            request,
            references,
            remote_id: None,
            generation_id: None,
            usage: None,
            artifact: None,
            error: None,
            output_text: None,
        };
        save_job(&dir, &mut job)?;
        job
    };
    // A later explicit cleanup request may strengthen this same upload's policy.
    if let Some(plan) = &upload {
        let mut changed = false;
        if requested_cleanup
            && let Some(reference) = job.references.first_mut()
            && reference["storage_upload"] == serde_json::to_value(&plan.record)?
            && reference["cleanup_requested"] != true
        {
            reference["cleanup_requested"] = json!(true);
            if reference["cleanup_status"] != "deleted" {
                reference["cleanup_status"] = json!("pending");
            }
            changed = true;
        }
        if changed {
            save_job(&dir, &mut job)?;
        }
    }
    if job.status == "prepared" {
        let owned;
        let transport = match transport {
            Some(t) => t,
            None => {
                owned = Transport::new()?;
                &owned
            }
        };
        // No remote storage work for completed, active, or uncertain jobs. Intent
        // already exists on disk before uploading, and before the paid POST.
        if let Some(plan) = &upload {
            let _storage_lock = crate::storage::operation_lock()?;
            crate::storage::ensure_uploaded(plan)?;
        }
        job.status = "submitting".into();
        save_job(&dir, &mut job)?;
        match transport.generated_response(&job.task, &body, &dir) {
            Ok(response) => {
                if is_synchronous_task(&job.task) {
                    crate::io::atomic_write(
                        &dir.join("response.json"),
                        &serde_json::to_vec(&response)?,
                        true,
                    )?;
                    job.usage = response.get("usage").cloned();
                    job.status = "received".into();
                    save_job(&dir, &mut job)?;
                    complete_synchronous(&dir, &mut job, &response)?;
                } else {
                    let remote = response["id"]
                        .as_str()
                        .context("submit response lacks provider job ID")?;
                    remote_id(remote)?;
                    job.remote_id = Some(remote.into());
                    job.status = "pending".into();
                    save_job(&dir, &mut job)?;
                    update_video(&dir, &mut job, &response)?;
                }
            }
            Err(error) => {
                recover_audio_generation_id(&dir, &mut job)?;
                let rejected = error.downcast_ref::<HttpRejection>().is_some_and(|e| {
                    matches!(e.status, 400 | 401 | 402 | 403 | 404 | 413 | 422 | 429)
                });
                job.status = if rejected { "rejected" } else { "uncertain" }.into();
                job.error = Some(if rejected {
                    format!(
                        "Submission rejected: {error:#}. Correct the request and use a new --take to explicitly retry this identity."
                    )
                } else {
                    format!(
                        "Submission outcome uncertain: {error:#}. Check OpenRouter activity; do not resubmit automatically. Adopt a known video ID with jobs resume --remote-id."
                    )
                });
                save_job(&dir, &mut job)?;
                bail!("{}; job {}", job.error.as_deref().unwrap(), job.id);
            }
        }
    }
    if job.status == "submitting"
        && is_synchronous_task(&job.task)
        && dir.join("response.json").is_file()
    {
        job.status = "received".into();
        save_job(&dir, &mut job)?;
    }
    if job.status == "submitting" {
        recover_audio_generation_id(&dir, &mut job)?;
        job.status = "uncertain".into();
        job.error=Some("Previous process stopped during submission. Check OpenRouter activity before intentionally creating a new take; adopt known video ID with jobs resume --remote-id.".into());
        save_job(&dir, &mut job)?;
    }
    if job.status == "received" {
        let response: Value = serde_json::from_slice(&limited(
            open_regular(&dir.join("response.json"), false)?,
            MAX_JSON,
        )?)?;
        complete_synchronous(&dir, &mut job, &response)?;
    }
    if job.remote_id.is_some() && job.status != "completed" && job.status != "failed" {
        let owned;
        let t = match transport {
            Some(t) => t,
            None => {
                owned = Transport::new()?;
                &owned
            }
        };
        refresh_video(&dir, &mut job, t, wait)?;
    }
    cleanup_job_uploads(&dir, &mut job)?;
    if let Some(output) = common.output.as_deref()
        && job.status == "completed"
    {
        publish(&dir, &job, output, common.overwrite)?;
    }
    public_job(&dir, &job)
}
fn complete_image(dir: &Path, job: &mut Job, response: &Value) -> Result<()> {
    let data = response["data"]
        .as_array()
        .context("image response lacks data array")?;
    if data.len() != 1 {
        bail!("expected exactly one image result");
    }
    let encoded = data[0]["b64_json"]
        .as_str()
        .context("image response lacks base64 data")?;
    if encoded.len() > MAX_IMAGE * 4 / 3 + 4 {
        bail!("image result exceeds 32 MiB");
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    if bytes.len() > MAX_IMAGE {
        bail!("image result exceeds 32 MiB");
    }
    let (mime, extension) = image_type(&bytes)?;
    let dimensions = imagesize::blob_size(&bytes)?;
    if dimensions.width == 0
        || dimensions.height == 0
        || (dimensions.width as u64).saturating_mul(dimensions.height as u64) > MAX_PIXELS
    {
        bail!("image result exceeds 64 megapixels");
    }
    if let Some(declared) = data[0]["media_type"].as_str()
        && declared != mime
    {
        bail!("provider image MIME disagrees with actual bytes");
    }
    let file = format!("result.{extension}");
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.write_all(&bytes)?;
    temp.as_file().sync_all()?;
    if let Err(error) = validate_generated_media(temp.path(), mime) {
        job.error = Some(format!("Provider image result is not decodable: {error:#}"));
        if let Some(usage) = response.get("usage") {
            job.usage = Some(usage.clone());
        }
        save_job(dir, job)?;
        bail!("{}; job {}", job.error.as_deref().unwrap(), job.id);
    }
    temp.persist(dir.join(&file)).map_err(|e| e.error)?;
    job.artifact = Some(Artifact {
        sha256: crate::media::file_sha256_limited(&dir.join(&file), bytes.len() as u64)?,
        file,
        media_type: mime.into(),
        bytes: bytes.len() as u64,
        decode_validation_version: 1,
    });
    job.usage = response.get("usage").cloned();
    job.status = "completed".into();
    job.error = None;
    save_job(dir, job)?;
    Ok(())
}
fn update_video(dir: &Path, job: &mut Job, response: &Value) -> Result<()> {
    let status = response["status"]
        .as_str()
        .context("video response lacks status")?;
    if !matches!(status, "pending" | "in_progress" | "completed" | "failed") {
        bail!("unknown video job status {status}");
    }
    job.status = if status == "completed" {
        "ready"
    } else {
        status
    }
    .into();
    if let Some(usage) = response.get("usage") {
        job.usage = Some(usage.clone());
    }
    if let Some(id) = response["generation_id"].as_str() {
        job.generation_id = Some(id.chars().take(256).collect());
    }
    if status == "failed" {
        job.error = Some(response["error"].to_string().chars().take(2048).collect());
    }
    save_job(dir, job)
}
fn refresh_video(dir: &Path, job: &mut Job, transport: &Transport, wait: u64) -> Result<()> {
    wait_check(wait)?;
    let start = Instant::now();
    loop {
        if job.status == "completed" || job.status == "failed" {
            return Ok(());
        }
        let id = job
            .remote_id
            .clone()
            .context("job has no provider ID; cannot resume an uncertain submission")?;
        remote_id(&id)?;
        if job.status != "ready" {
            let response = transport.json(&format!("/videos/{id}"), None)?;
            update_video(dir, job, &response)?;
        }
        if job.status == "ready" {
            let temp = tempfile::NamedTempFile::new_in(dir)?;
            transport.content(&id, temp.path())?;
            let media = crate::media::probe(temp.path())?;
            if !media.has_video()
                || !media
                    .format_name
                    .split(',')
                    .any(|s| matches!(s, "mov" | "mp4"))
            {
                bail!("provider output is not an MP4 video");
            }
            if media.video().is_some_and(|s| {
                u64::from(s.width.unwrap_or(0)) * u64::from(s.height.unwrap_or(0)) > MAX_PIXELS
            }) {
                bail!("video output exceeds 64 megapixels per frame");
            }
            if let Err(error) = validate_generated_media(temp.path(), "video/mp4") {
                job.error = Some(format!("Provider video result is not decodable: {error:#}"));
                save_job(dir, job)?;
                bail!("{}; job {}", job.error.as_deref().unwrap(), job.id);
            }
            let sha256 = crate::media::file_sha256_limited(temp.path(), MAX_VIDEO)?;
            let bytes = temp.as_file().metadata()?.len();
            temp.persist(dir.join("result.mp4")).map_err(|e| e.error)?;
            job.artifact = Some(Artifact {
                file: "result.mp4".into(),
                sha256,
                media_type: "video/mp4".into(),
                bytes,
                decode_validation_version: 1,
            });
            job.status = "completed".into();
            job.error = None;
            save_job(dir, job)?;
            return Ok(());
        }
        if job.status == "failed" || start.elapsed() >= Duration::from_secs(wait) {
            return Ok(());
        }
        std::thread::sleep(
            Duration::from_secs(2).min(Duration::from_secs(wait).saturating_sub(start.elapsed())),
        );
    }
}
/// Persist every directory link, deepest first, including newly created parents.
/// Call after creation/rename and before recording durable dependent state.
pub(crate) fn sync_directory_chain(path: &Path) -> Result<()> {
    #[cfg(unix)]
    for ancestor in path.canonicalize()?.ancestors() {
        File::open(ancestor)?.sync_all()?;
    }
    Ok(())
}
/// Recreate only a genuinely missing publication from an already completed,
/// hash-verified local job. Compare converted bytes before publishing anything.
pub(crate) fn restore_project_publication(id: &str, output: &Path, hash: &str) -> Result<()> {
    let (dir, _lock) = locked_job(id)?;
    let job = load_job(&dir)?;
    if job.status != "completed" {
        bail!("completed project publication lacks a completed local job");
    }
    let scratch = tempfile::tempdir()?;
    let expected = scratch
        .path()
        .join(output.file_name().context("output has no filename")?);
    publish(&dir, &job, &expected, false)?;
    if crate::media::file_sha256(&expected)? != hash {
        bail!("local job publication disagrees with completed project hash");
    }
    // publish uses kernel no-clobber even if a file appeared since absence check.
    publish(&dir, &job, output, false)
}
/// Reconcile publication after a project crash without touching the existing output.
/// Conversion can change bytes, so compare against the same publisher's result,
/// rather than the provider artifact hash. The project lock precedes this job lock.
pub(crate) fn verify_project_publication(id: &str, output: &Path) -> Result<Value> {
    let (dir, _lock) = locked_job(id)?;
    let job = load_job(&dir)?;
    if job.status != "completed" {
        bail!("existing output belongs to an unfinished job");
    }
    let scratch = tempfile::tempdir()?;
    let expected = scratch
        .path()
        .join(output.file_name().context("output has no filename")?);
    publish(&dir, &job, &expected, false)?;
    if crate::media::file_sha256_limited(output, fs::metadata(&expected)?.len())?
        != crate::media::file_sha256(&expected)?
    {
        bail!("existing output does not match recorded job publication");
    }
    File::open(output)?.sync_all()?;
    sync_directory_chain(output.parent().unwrap_or(Path::new(".")))?;
    public_job(&dir, &job)
}
fn publish(dir: &Path, job: &Job, output: &Path, overwrite: bool) -> Result<()> {
    validate_output(Some(output), &job.references, overwrite)?;
    let artifact = job
        .artifact
        .as_ref()
        .context("completed job lacks artifact")?;
    let source = verified_artifact_path(dir, artifact)?;
    if output.exists() && output.canonicalize()? == source.canonicalize()? {
        bail!("publication path is protected job artifact");
    }
    let extension = output
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    if is_audio_task_mime(&artifact.media_type) {
        return publish_audio(&source, output, &extension, overwrite);
    }
    if artifact.media_type == "video/mp4" && extension != "mp4" {
        bail!("video publication requires .mp4");
    }
    if artifact.media_type != "video/mp4" && extension == "mp4" {
        bail!("image publication requires PNG/JPEG/WebP");
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = tempfile::Builder::new()
        .suffix(&format!(".{extension}"))
        .tempfile_in(parent)?;
    let same = matches!(
        (artifact.media_type.as_str(), extension.as_str()),
        ("image/png", "png")
            | ("image/jpeg", "jpg" | "jpeg")
            | ("image/webp", "webp")
            | ("video/mp4", "mp4")
    );
    if same {
        fs::copy(&source, temp.path())?;
    } else {
        let mut args = crate::process::args(&[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
        ]);
        args.push(source.into_os_string());
        args.extend(crate::process::args(&["-frames:v", "1"]));
        args.push(temp.path().as_os_str().to_owned());
        crate::process::run("ffmpeg", &args, Duration::from_secs(60), 1024 * 1024)?;
    }
    temp.as_file().sync_all()?;
    if overwrite {
        temp.persist(output).map_err(|e| e.error)?;
    } else {
        temp.persist_noclobber(output).map_err(|e| e.error)?;
    }
    sync_directory_chain(parent)?;
    Ok(())
}
/// Read atomic job snapshots while holding the storage lock; never take job locks here.
pub(crate) fn active_storage_uploads(
    exclude: Option<&str>,
) -> Result<std::collections::HashSet<String>> {
    let root = jobs_root()?;
    let mut active = std::collections::HashSet::new();
    if !job_path_exists(&root)? {
        return Ok(active);
    }
    if !fs::symlink_metadata(&root)?.is_dir() {
        bail!("generation jobs root must be a non-symlink directory");
    }
    for (index, entry) in fs::read_dir(root)?.enumerate() {
        if index >= 10000 {
            bail!("active upload scan exceeds 10000 job entries; no objects were deleted");
        }
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str() else { continue };
        if exclude == Some(id) || valid_id(id).is_err() {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!(
                "cannot inspect symlink generation job directory {}",
                entry.path().display()
            );
        }
        if !kind.is_dir() || !job_path_exists(&entry.path().join("job.json"))? {
            continue;
        }
        let job = load_job(&entry.path())?;
        if matches!(job.status.as_str(), "completed" | "failed" | "rejected") {
            continue;
        }
        for reference in job.references {
            if let Some(value) = reference.get("storage_upload") {
                let record: crate::storage::UploadRecord = serde_json::from_value(value.clone())?;
                record.validate()?;
                active.insert(record.identity()?);
            }
        }
    }
    Ok(active)
}
fn cleanup_job_uploads(dir: &Path, job: &mut Job) -> Result<()> {
    // Even known failures retain uploads: callers can reuse or explicitly purge them.
    if job.status != "completed" {
        return Ok(());
    }
    let candidates: Vec<_> = job
        .references
        .iter()
        .enumerate()
        .filter_map(|(index, reference)| {
            (reference["cleanup_requested"] == true && reference["cleanup_status"] != "deleted")
                .then(|| {
                    reference
                        .get("storage_upload")
                        .cloned()
                        .map(|record| (index, record))
                })
                .flatten()
        })
        .collect();
    if candidates.is_empty() {
        return Ok(());
    }
    verified_artifact_path(
        dir,
        job.artifact
            .as_ref()
            .context("completed job lacks artifact")?,
    )?;
    for (index, value) in candidates {
        let result = (|| -> Result<bool> {
            let record: crate::storage::UploadRecord = serde_json::from_value(value)?;
            record.validate()?;
            let _storage_lock = crate::storage::operation_lock()?;
            if active_storage_uploads(Some(&job.id))?.contains(&record.identity()?) {
                return Ok(false);
            }
            crate::storage::delete_upload(&record)?;
            Ok(true)
        })();
        let reference = &mut job.references[index];
        match result {
            Ok(true) => {
                reference["cleanup_status"] = json!("deleted");
                reference.as_object_mut().unwrap().remove("cleanup_error");
            }
            Ok(false) => {
                reference["cleanup_status"] = json!("deferred_active_job");
            }
            Err(error) => {
                reference["cleanup_status"] = json!("failed");
                reference["cleanup_error"] =
                    json!(format!("{error:#}").chars().take(1024).collect::<String>());
            }
        }
        save_job(dir, job)?;
    }
    Ok(())
}
pub fn execute_jobs(command: JobsCommand) -> Result<Value> {
    match command {
        JobsCommand::List => {
            let root = jobs_root()?;
            if !job_path_exists(&root)? {
                return Ok(json!({"jobs":[]}));
            }
            let mut jobs = Vec::new();
            for (n, entry) in fs::read_dir(root)?.enumerate() {
                if n >= 10000 {
                    bail!("job listing exceeds 10000 entries");
                }
                let entry = entry?;
                let name = entry.file_name();
                let Some(id) = name.to_str() else { continue };
                if valid_id(id).is_ok()
                    && entry.file_type()?.is_dir()
                    && job_path_exists(&entry.path().join("job.json"))?
                {
                    let job = load_job(&entry.path())?;
                    jobs.push(json!({"id":job.id,"task":job.task,"model":job.model,"status":job.status,"created_at":job.created_at}));
                }
            }
            jobs.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
            Ok(json!({"jobs":jobs}))
        }
        JobsCommand::Status { id, refresh } => {
            let (dir, _lock) = locked_job(&id)?;
            let mut job = load_job(&dir)?;
            if refresh
                && job.remote_id.is_some()
                && job.status != "completed"
                && job.status != "failed"
            {
                refresh_video(&dir, &mut job, &Transport::new()?, 0)?;
            }
            if refresh
                && is_audio_task(&job.task)
                && job.usage.as_ref().is_none_or(Value::is_null)
                && let Some(id) = &job.generation_id
                && let Some(usage) = Transport::new()?.billing(id)?
            {
                job.usage = Some(usage);
                save_job(&dir, &mut job)?;
            }
            if refresh {
                cleanup_job_uploads(&dir, &mut job)?;
            }
            public_job(&dir, &job)
        }
        JobsCommand::Resume {
            id,
            output,
            overwrite,
            wait,
            remote_id: adopt,
        } => {
            wait_check(wait)?;
            let (dir, _lock) = locked_job(&id)?;
            let mut job = load_job(&dir)?;
            validate_output(output.as_deref(), &job.references, overwrite)?;
            if let Some(id) = adopt {
                remote_id(&id)?;
                if is_synchronous_task(&job.task)
                    || job.remote_id.is_some()
                    || !matches!(job.status.as_str(), "submitting" | "uncertain")
                {
                    bail!(
                        "provider ID adoption only allowed for uncertain video submits without known ID"
                    );
                }
                job.remote_id = Some(id);
                job.status = "pending".into();
                job.error = None;
                save_job(&dir, &mut job)?;
            }
            if is_synchronous_task(&job.task)
                && job.status == "submitting"
                && dir.join("response.json").is_file()
            {
                job.status = "received".into();
                save_job(&dir, &mut job)?;
            }
            if job.status == "received" {
                let value: Value = serde_json::from_slice(&limited(
                    open_regular(&dir.join("response.json"), false)?,
                    MAX_JSON,
                )?)?;
                complete_synchronous(&dir, &mut job, &value)?;
            }
            if is_synchronous_task(&job.task) && job.status == "submitting" {
                recover_audio_generation_id(&dir, &mut job)?;
                job.status = "uncertain".into();
                job.error=Some("Previous process stopped during paid submission; check OpenRouter activity and do not blindly repeat it.".into());
                save_job(&dir, &mut job)?;
            }
            if !is_synchronous_task(&job.task)
                && !matches!(job.status.as_str(), "completed" | "failed" | "rejected")
            {
                refresh_video(&dir, &mut job, &Transport::new()?, wait)?;
            }
            cleanup_job_uploads(&dir, &mut job)?;
            if let Some(output) = output
                && job.status == "completed"
            {
                publish(&dir, &job, &output, overwrite)?;
            }
            public_job(&dir, &job)
        }
    }
}
pub fn execute_upscale(command: UpscaleCommand) -> Result<Value> {
    match command {
        UpscaleCommand::Image {
            input,
            output,
            factor,
            overwrite,
        } => resize_image(&input, &output, factor, overwrite),
        UpscaleCommand::Video(args) => {
            wait_check(args.wait)?;
            if !args.factor.is_finite() || args.factor <= 1.0 || args.creativity > 1 {
                bail!("factor must be finite and >1; creativity must be 0 or 1");
            }
            if let Some(prompt) = &args.prompt {
                prompt_check(prompt)?;
            }
            let model =
                crate::models::resolve_model("video-upscaling", args.common.model.as_deref())?;
            let caps = crate::models::find_model(
                "video-upscaling",
                &model,
                args.common.cache_dir.as_deref(),
                args.common.offline,
            )?;
            let range = &caps["upscale_factor"];
            if range.is_null()
                || args.factor < range["min"].as_f64().unwrap_or(f64::INFINITY)
                || args.factor > range["max"].as_f64().unwrap_or(f64::NEG_INFINITY)
            {
                bail!("factor outside selected model capability range");
            }
            if !caps["creativity"]
                .as_array()
                .is_some_and(|v| v.contains(&json!(args.creativity)))
            {
                bail!("model does not support selected creativity");
            }
            let upload = if args.upload {
                Some(crate::storage::plan_upload(
                    args.input
                        .as_deref()
                        .context("--upload requires a local MP4 input")?,
                )?)
            } else {
                None
            };
            let (source_url, references) = if let Some(plan) = &upload {
                plan.protect_output(args.common.output.as_deref())?;
                let config_path = crate::storage::config_path()?.canonicalize()?;
                (
                    plan.record.public_url()?,
                    vec![
                        json!({"path":plan.source,"sha256":plan.record.sha256,"bytes":plan.record.bytes,
                    "media_type":"video/mp4","storage_upload":plan.record,"cleanup_requested":args.delete_upload_after,
                    "cleanup_status":if args.delete_upload_after {"pending"} else {"retained"}}),
                        json!({"path":config_path,"kind":"storage_configuration"}),
                    ],
                )
            } else {
                upscale_source(&args)?
            };
            validate_public_url(&url::Url::parse(&source_url)?)?;
            let mut body = json!({"model":model,"upscale_factor":args.factor,"creativity":args.creativity,"input_references":[{"type":"video_url","video_url":{"url":source_url}}]});
            if let Some(prompt) = args.prompt {
                body["prompt"] = json!(prompt);
            }
            submit_with_upload(
                Prepared {
                    task: "video-upscaling".into(),
                    model,
                    body,
                    references,
                    common: args.common,
                    wait: args.wait,
                },
                None,
                upload,
            )
        }
    }
}
fn upscale_source(args: &VideoUpscaleArgs) -> Result<(String, Vec<Value>)> {
    let (url, mut reference) = if let Some(id) = &args.source_job {
        let (dir, _lock) = locked_job(id)?;
        let job = load_job(&dir)?;
        let artifact = job
            .artifact
            .as_ref()
            .filter(|a| a.media_type == "video/mp4")
            .context("source job must have a completed MP4 artifact")?;
        if job.status != "completed" {
            bail!("source job must be completed");
        }
        let path = verified_artifact_path(&dir, artifact)?;
        let remote = job
            .remote_id
            .as_deref()
            .context("source job has no provider ID")?;
        let status = Transport::new()?.json(&format!("/videos/{remote}"), None)?;
        let url=status["urls"][0].as_str().context("OpenRouter has no signed public URL for this video; use --source-url for a hosted copy")?.to_owned();
        (
            url,
            json!({"source_job":id,"sha256":artifact.sha256,"path":path,"bytes":artifact.bytes,"media_type":"video/mp4"}),
        )
    } else if let Some(url) = &args.source_url {
        let parsed = url::Url::parse(url)?;
        validate_public_url(&parsed)?;
        if url.len() > 16384 {
            bail!("source URL exceeds 16 KiB");
        }
        let visible = format!("{}{}", parsed.origin().ascii_serialization(), parsed.path());
        (
            url.clone(),
            json!({"source_url":visible,"sha256":bytes_sha256(url.as_bytes()),"identity_kind":"url-sha256","media_type":"video/mp4"}),
        )
    } else {
        bail!(
            "OpenRouter video upscaling accepts HTTPS sources only. Use --source-job ID for generated video or --source-url for your hosted file; or --upload with a configured public S3/R2 bucket for local MP4 files."
        );
    };
    validate_public_url(&url::Url::parse(&url)?)?;
    if let Some(input) = &args.input {
        let path = crate::media::canonical_file(input)?;
        let info = crate::media::probe(&path)?;
        if !info.has_video() {
            bail!("local source must contain video");
        }
        let sha = crate::media::file_sha256(&path)?;
        if args.source_job.is_some() && reference["sha256"] != sha {
            bail!("local file does not match source job artifact");
        }
        reference["path"] = json!(path);
        reference["local_sha256"] = json!(sha);
    }
    Ok((url, vec![reference]))
}
fn resize_image(input: &Path, output: &Path, factor: f64, overwrite: bool) -> Result<Value> {
    if !factor.is_finite() || !(1.0..=8.0).contains(&factor) {
        bail!("local image scale must be between 1 and 8");
    }
    let input = crate::media::canonical_file(input)?;
    let (bytes, _) = image_bytes(&input)?;
    let size = imagesize::blob_size(&bytes)?;
    let width = (size.width as f64 * factor).round() as u64;
    let height = (size.height as f64 * factor).round() as u64;
    if width.saturating_mul(height) > MAX_PIXELS {
        bail!("resized image exceeds 64 megapixels");
    }
    let references = vec![json!({"path":input})];
    validate_output(Some(output), &references, overwrite)?;
    let ext = output
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    if ext == "mp4" {
        bail!("image resize requires PNG/JPEG/WebP output");
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = tempfile::Builder::new()
        .suffix(&format!(".{ext}"))
        .tempfile_in(parent)?;
    let filter = format!("scale={width}:{height}:flags=lanczos");
    let mut args = crate::process::args(&[
        "-v",
        "error",
        "-nostdin",
        "-y",
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]);
    args.push(input.clone().into_os_string());
    args.extend(crate::process::args(&["-vf", &filter, "-frames:v", "1"]));
    args.push(temp.path().as_os_str().to_owned());
    crate::process::run("ffmpeg", &args, Duration::from_secs(60), 1024 * 1024)?;
    temp.as_file().sync_all()?;
    if overwrite {
        temp.persist(output).map_err(|e| e.error)?;
    } else {
        temp.persist_noclobber(output).map_err(|e| e.error)?;
    }
    Ok(
        json!({"input":input,"output":output,"width":width,"height":height,"factor":factor,"method":"ffmpeg-lanczos","ai":false,"paid_request":false,"sha256":crate::media::file_sha256(output)?}),
    )
}

fn recover_audio_generation_id(dir: &Path, job: &mut Job) -> Result<()> {
    let headers = dir.join("audio-response-headers.json");
    if is_audio_task(&job.task) && job.generation_id.is_none() && headers.exists() {
        let metadata: Value =
            serde_json::from_slice(&limited(open_regular(&headers, false)?, 16 * 1024)?)?;
        if let Some(id) = metadata["generation_id"].as_str().filter(|s| !s.is_empty()) {
            remote_id(id)?;
            job.generation_id = Some(id.into());
        }
    }
    Ok(())
}
fn is_audio_task(task: &str) -> bool {
    matches!(task, "speech-generation" | "music-generation")
}
fn is_audio_task_mime(mime: &str) -> bool {
    matches!(mime, "audio/mpeg" | "audio/wav")
}
fn is_synchronous_task(task: &str) -> bool {
    task == "image-generation" || is_audio_task(task)
}
pub(crate) fn read_bounded_file(path: &Path, max: usize) -> Result<Vec<u8>> {
    limited(open_regular(path, false)?, max)
}
pub(crate) fn audio_image_reference(path: &Path, refs: &mut Vec<Value>) -> Result<Value> {
    let part = add_image(path, refs)?;
    validate_generated_media(
        path,
        refs.last()
            .and_then(|v| v["media_type"].as_str())
            .context("image reference lacks MIME")?,
    )?;
    Ok(part)
}
pub(crate) fn submit_audio(
    task: &str,
    model: String,
    body: Value,
    references: Vec<Value>,
    common: Common,
) -> Result<Value> {
    if !is_audio_task(task) {
        bail!("invalid synchronous audio task");
    }
    submit(Prepared {
        task: task.into(),
        model,
        body,
        references,
        common,
        wait: 0,
    })
}
fn complete_synchronous(dir: &Path, job: &mut Job, response: &Value) -> Result<()> {
    if job.task == "image-generation" {
        return complete_image(dir, job, response);
    }
    if !is_audio_task(&job.task) {
        bail!("invalid synchronous result task");
    }
    let audio = crate::audio_generation::audio_data(response)?;
    let mut temp = tempfile::Builder::new()
        .prefix("audio-")
        .suffix(&format!(".{}", audio.extension))
        .tempfile_in(dir)?;
    temp.write_all(&audio.bytes)?;
    temp.as_file().sync_all()?;
    crate::audio_generation::validate_audio(temp.path(), audio.media_type)?;
    let file = format!("result.{}", audio.extension);
    temp.persist(dir.join(&file)).map_err(|e| e.error)?;
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    job.artifact = Some(Artifact {
        file,
        sha256: bytes_sha256(&audio.bytes),
        media_type: audio.media_type.into(),
        bytes: audio.bytes.len() as u64,
        decode_validation_version: 1,
    });
    if let Some(id) = response["generation_id"].as_str().filter(|v| !v.is_empty()) {
        remote_id(id)?;
        job.generation_id = Some(id.into());
    }
    if let Some(usage) = response.get("usage").filter(|v| !v.is_null()) {
        job.usage = Some(usage.clone());
    }
    job.output_text = response["text"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    job.status = "completed".into();
    job.error = None;
    save_job(dir, job)
}
fn publish_audio(source: &Path, output: &Path, extension: &str, overwrite: bool) -> Result<()> {
    if !matches!(extension, "mp3" | "wav") {
        bail!("audio publication requires .mp3 or .wav");
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = tempfile::Builder::new()
        .suffix(&format!(".{extension}"))
        .tempfile_in(parent)?;
    if source.extension().and_then(|v| v.to_str()) == Some(extension) {
        fs::copy(source, temp.path())?;
    } else {
        let mut args = crate::process::args(&[
            "-v",
            "error",
            "-nostdin",
            "-xerror",
            "-y",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
        ]);
        args.push(source.as_os_str().to_owned());
        args.extend(crate::process::args(&["-map", "0:a:0", "-vn"]));
        if extension == "wav" {
            args.extend(crate::process::args(&[
                "-acodec",
                "pcm_s16le",
                "-ar",
                "48000",
                "-ac",
                "2",
            ]));
        }
        args.push(temp.path().as_os_str().to_owned());
        crate::process::run("ffmpeg", &args, Duration::from_secs(180), 1024 * 1024)?;
    }
    if temp.as_file().metadata()?.len() > 128 * 1024 * 1024 {
        bail!("published audio exceeds 128 MiB");
    }
    temp.as_file().sync_all()?;
    if overwrite {
        temp.persist(output).map_err(|e| e.error)?;
    } else {
        temp.persist_noclobber(output).map_err(|e| e.error)?;
    }
    sync_directory_chain(parent)?;
    Ok(())
}
impl Transport {
    fn generated_response(&self, task: &str, body: &Value, dir: &Path) -> Result<Value> {
        if !is_audio_task(task) {
            return self.json(
                if task == "image-generation" {
                    "/images"
                } else {
                    "/videos"
                },
                Some(body),
            );
        }
        let endpoint = if task == "speech-generation" {
            "/audio/speech"
        } else {
            "/chat/completions"
        };
        let encoded = serde_json::to_vec(body)?;
        if encoded.len() > MAX_REQUEST {
            bail!("audio request exceeds 64 MiB");
        }
        let response = self
            .client
            .post(format!("{}{endpoint}", self.base))
            .bearer_auth(&self.key)
            .header("Content-Type", "application/json")
            .header("X-Title", "Skywrite")
            .body(encoded)
            .send()?;
        let status = response.status();
        if !status.is_success() {
            let bytes = limited(response, MAX_JSON)?;
            return Err(HttpRejection {
                status: status.as_u16(),
                message: self.diagnostic(&String::from_utf8_lossy(&bytes), Some(body)),
            }
            .into());
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let generation_id = response
            .headers()
            .get("x-generation-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        if !generation_id.is_empty() {
            remote_id(&generation_id)?;
        }
        crate::io::write_json(
            &dir.join("audio-response-headers.json"),
            &json!({"generation_id":generation_id,"content_type":content_type}),
            true,
        )?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        if task == "music-generation" {
            if !content_type
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
            {
                bail!("music response was not the requested event stream");
            }
            return crate::audio_generation::parse_music_stream(response);
        }
        if !content_type.to_ascii_lowercase().starts_with("audio/") {
            bail!("speech response did not contain audio");
        }
        let bytes = limited(response, crate::audio_generation::MAX_AUDIO)?;
        let usage = if !generation_id.is_empty() {
            self.billing(&generation_id).ok().flatten()
        } else {
            None
        };
        Ok(
            json!({"audio":{"data":base64::engine::general_purpose::STANDARD.encode(bytes)},"generation_id":generation_id,"provider_content_type":content_type,"usage":usage}),
        )
    }
    /// Billing lookup is optional metadata only; absence is unknown cost, never zero.
    fn billing(&self, id: &str) -> Result<Option<Value>> {
        remote_id(id)?;
        let result = self.json(&format!("/generation?id={id}"), None)?;
        Ok(result["data"]["total_cost"]
            .as_f64()
            .map(|cost| json!({"cost":cost,"source":"OpenRouter generation metadata"})))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn public_address_registry_endpoints_and_inverse() {
        let table = include_str!("../tests/fixtures/network/endpoints.txt");
        let mut count = 0;
        for line in table.lines().filter(|line| !line.starts_with('#')) {
            let cells: Vec<_> = line.split_whitespace().collect();
            for address in &cells[1..=2] {
                assert_eq!(
                    public_address(address.parse().unwrap()),
                    cells[3] == "true",
                    "{} ({})",
                    address,
                    cells[0]
                );
            }
            count += 1;
        }
        assert_eq!(count, 51);
        for address in [
            "1.1.1.1",
            "8.8.8.8",
            "192.1.0.1",
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
            "3ffe:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "3fff:1000::",
        ] {
            assert!(public_address(address.parse().unwrap()), "{address}");
        }
        for address in [
            "224.0.0.1",
            "ff02::1",
            "::ffff:8.8.8.8",
            "2002:808:808::",
            "64:ff9b::808:808",
            "::ffff:127.0.0.1",
        ] {
            assert!(!public_address(address.parse().unwrap()), "{address}");
        }
        for raw in [
            "http://example.com/",
            "https://user:pass@example.com/",
            "https://example.com:444/",
            "https://example.com/#fragment",
            "https://localhost/",
            "https://x.localhost/",
        ] {
            assert!(
                validate_public_url(&url::Url::parse(raw).unwrap()).is_err(),
                "{raw}"
            );
        }
        for raw in [
            "https://127.0.0.1/",
            "https://2130706433/",
            "https://0x7f000001/",
            "https://[::1]/",
            "https://[::ffff:127.0.0.1]/",
        ] {
            let error = public_download_client(&url::Url::parse(raw).unwrap()).unwrap_err();
            assert!(error.to_string().contains("nonpublic"), "{raw}: {error}");
        }
        let v6 = url::Url::parse("https://[2606:4700:4700::1111]/").unwrap();
        assert!(matches!(v6.host(), Some(url::Host::Ipv6(_))));
        assert!(public_download_client(&v6).is_ok()); // Builds only; no request.
    }

    // A bounded owned Python TLS server. Drop always kills/reaps the fixture.
    struct OwnedTls {
        child: std::process::Child,
        address: std::net::SocketAddr,
    }
    impl Drop for OwnedTls {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    impl OwnedTls {
        fn start(ip: &str, port: u16) -> Self {
            Self::start_with_certificate(ip, port, "test-server.pem")
        }
        fn start_with_certificate(ip: &str, port: u16, certificate: &str) -> Self {
            use std::io::BufRead;
            let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/network");
            let mut child = std::process::Command::new("python3")
                .arg(base.join("tls_server.py"))
                .arg(base.join(certificate))
                .arg(base.join("test-server.key"))
                .arg(ip)
                .arg(port.to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            let address =
                std::net::SocketAddr::new(ip.parse().unwrap(), line.trim().parse().unwrap());
            Self { child, address }
        }
        fn client(&self, host: &str) -> Client {
            let cert = reqwest::Certificate::from_pem(include_bytes!(
                "../tests/fixtures/network/test-ca.pem"
            ))
            .unwrap();
            pinned_download_builder(host, &[self.address])
                .tls_certs_merge([cert])
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap()
        }
    }
    #[test]
    fn pinned_download_owned_tls_socket_and_certificate_boundary() {
        // localhost has a real fallback address. With trusted matching TLS,
        // a forbidden resolver fallback would turn the wrong-pin failure into
        // success against this live server.
        let fallback = OwnedTls::start("127.0.0.1", 0);
        let fallback_url = format!("https://localhost:{}/", fallback.address.port());
        assert!(
            fallback
                .client("localhost")
                .get(&fallback_url)
                .send()
                .unwrap()
                .status()
                .is_success()
        );
        let unavailable =
            std::net::SocketAddr::new("127.0.0.2".parse().unwrap(), fallback.address.port());
        let cert =
            reqwest::Certificate::from_pem(include_bytes!("../tests/fixtures/network/test-ca.pem"))
                .unwrap();
        let no_fallback = pinned_download_builder("localhost", &[unavailable])
            .tls_certs_merge([cert])
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(no_fallback.get(&fallback_url).send().is_err());
        let first = OwnedTls::start("127.0.0.1", 0);
        let port = first.address.port();
        let second = OwnedTls::start("127.0.0.2", port);
        let domain_url = format!("https://pinned.invalid:{port}/");
        let client = first.client("pinned.invalid");
        assert_eq!(
            client.get(&domain_url).send().unwrap().text().unwrap(),
            "ok"
        );
        // Rebuilding with the same origin but a different address reaches server 2.
        // Killing server 1 ensures an old pool or ignored new pin cannot pass.
        drop(first);
        assert!(client.get(&domain_url).send().is_err());
        assert_eq!(
            second
                .client("pinned.invalid")
                .get(&domain_url)
                .send()
                .unwrap()
                .text()
                .unwrap(),
            "ok"
        );
        let redirect = second
            .client("pinned.invalid")
            .get(format!("https://pinned.invalid:{port}/redirect"))
            .send()
            .unwrap();
        assert_eq!(redirect.status(), reqwest::StatusCode::FOUND);
        // A wrong pin must fail even while a valid same-port loopback server exists.
        let wrong: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let cert =
            reqwest::Certificate::from_pem(include_bytes!("../tests/fixtures/network/test-ca.pem"))
                .unwrap();
        let wrong_client = pinned_download_builder("pinned.invalid", &[wrong])
            .tls_certs_merge([cert])
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(wrong_client.get(&domain_url).send().is_err());
        assert!(
            second
                .client("mismatch.invalid")
                .get(format!("https://mismatch.invalid:{port}/"))
                .send()
                .is_err()
        );
        assert_eq!(
            second
                .client("127.0.0.2")
                .get(format!("https://127.0.0.2:{port}/"))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        let v6 = OwnedTls::start("::1", 0);
        assert_eq!(
            v6.client("::1")
                .get(format!("https://[::1]:{}/", v6.address.port()))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        for ip in ["127.0.0.1", "::1"] {
            let mismatched = OwnedTls::start_with_certificate(ip, 0, "test-mismatch.pem");
            let literal = if ip.contains(':') {
                format!("[{ip}]")
            } else {
                ip.to_owned()
            };
            assert!(
                mismatched
                    .client(ip)
                    .get(format!("https://{literal}:{}/", mismatched.address.port()))
                    .send()
                    .is_err()
            );
            // Same certificate and socket succeed for its DNS SAN: rejection
            // above is specifically the missing IP SAN, not broken trust/I/O.
            assert_eq!(
                mismatched
                    .client("mismatch.invalid")
                    .get(format!(
                        "https://mismatch.invalid:{}/",
                        mismatched.address.port()
                    ))
                    .send()
                    .unwrap()
                    .status(),
                reqwest::StatusCode::OK
            );
        }
        // Name mismatch remains rejected on an IPv6 connection.
        assert!(
            v6.client("mismatch.invalid")
                .get(format!("https://mismatch.invalid:{}/", v6.address.port()))
                .send()
                .is_err()
        );
    }

    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    fn common() -> Common {
        Common {
            model: None,
            output: None,
            overwrite: false,
            cache_dir: None,
            offline: false,
            dry_run: false,
            take: None,
        }
    }
    fn prepared(task: &str, prompt: &str) -> Prepared {
        Prepared {
            task: task.into(),
            model: "test/model".into(),
            body: json!({"model":"test/model","prompt":prompt}),
            references: vec![],
            common: common(),
            wait: 0,
        }
    }
    fn fixture_png() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jQZkAAAAASUVORK5CYII=").unwrap()
    }
    fn image_response() -> Value {
        json!({"data":[{"media_type":"image/png","b64_json":base64::engine::general_purpose::STANDARD.encode(fixture_png())}],"usage":{"cost":0.008}})
    }
    /// Actual HTTP boundary; no paid request, no third-party listener.
    fn server(
        responses: Vec<Vec<u8>>,
        inspect_intent: bool,
    ) -> (
        Transport,
        Arc<Mutex<Vec<String>>>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let handle = std::thread::spawn(move || {
            for response in responses {
                let started = Instant::now();
                let (mut socket, _) = loop {
                    match listener.accept() {
                        Ok(v) => break v,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                started.elapsed() < Duration::from_secs(15),
                                "fixture missing expected request"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                let header_end = loop {
                    let n = socket.read(&mut buffer).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let header = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
                let length = header
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|s| s.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let n = socket.read(&mut buffer).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                }
                if header.starts_with("GET /download ") {
                    assert!(!header.to_lowercase().contains("authorization:"));
                } else {
                    assert!(
                        header
                            .to_lowercase()
                            .contains("authorization: bearer dummy-generation-test")
                    );
                }
                observed
                    .lock()
                    .unwrap()
                    .push(header.lines().next().unwrap().to_owned());
                if inspect_intent && header.starts_with("POST") {
                    let root = jobs_root().unwrap();
                    assert!(
                        fs::read_dir(root).unwrap().filter_map(Result::ok).any(|e| e
                            .path()
                            .join("job.json")
                            .is_file()
                            && load_job(&e.path()).unwrap().status == "submitting"),
                        "intent must be durable before observed HTTP POST"
                    );
                }
                if !response.is_empty() {
                    socket.write_all(&response).unwrap();
                }
            }
        });
        (
            Transport {
                client: Transport::client().unwrap(),
                key: "DUMMY-GENERATION-TEST".into(),
                base: format!("http://{address}"),
            },
            seen,
            handle,
        )
    }
    fn json_http(value: Value) -> Vec<u8> {
        let body = serde_json::to_vec(&value).unwrap();
        [format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).into_bytes(),body].concat()
    }
    #[test]
    fn durable_jobs_boundary() {
        if std::env::var_os("SKYWRITE_GENERATION_TEST_CHILD").is_none() {
            let temp = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "generation::tests::durable_jobs_boundary",
                    "--nocapture",
                ])
                .env("SKYWRITE_GENERATION_TEST_CHILD", "1")
                .env("SKYWRITE_HOME", temp.path())
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let (t, seen, handle) = server(vec![json_http(image_response())], true);
        let t = Arc::new(t);
        let mut threads = Vec::new();
        for _ in 0..6 {
            let t = t.clone();
            threads.push(std::thread::spawn(move || {
                submit_with(prepared("image-generation", "concurrent"), Some(&t)).unwrap()
            }));
        }
        let jobs = threads
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>();
        handle.join().unwrap();
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "concurrent identical request charged once"
        );
        assert!(
            jobs.iter()
                .all(|j| j["status"] == "completed" && j["id"] == jobs[0]["id"])
        );
        let id = jobs[0]["id"].as_str().unwrap();
        let dir = jobs_root().unwrap().join(id);
        let mut job = load_job(&dir).unwrap();
        // A saved response can recover even when process died before saving received state.
        job.status = "submitting".into();
        job.artifact = None;
        save_job(&dir, &mut job).unwrap();
        fs::remove_file(dir.join("result.png")).unwrap();
        assert_eq!(
            submit_with(prepared("image-generation", "concurrent"), Some(&t)).unwrap()["status"],
            "completed"
        );
        // Existing publication and original reference bytes remain unchanged on rejection.
        let output = dir.join("keep.png");
        fs::write(&output, b"original").unwrap();
        assert!(publish(&dir, &load_job(&dir).unwrap(), &output, false).is_err());
        assert_eq!(fs::read(output).unwrap(), b"original");
        let (t, seen, handle) = server(vec![Vec::new()], true);
        assert!(submit_with(prepared("video-generation", "uncertain"), Some(&t)).is_err());
        handle.join().unwrap();
        let retry = submit_with(prepared("video-generation", "uncertain"), Some(&t)).unwrap();
        assert_eq!(retry["status"], "uncertain");
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "ambiguous transport outcome must never automatically resubmit"
        );
        let rejection=b"HTTP/1.1 400 Bad Request\r\nContent-Length: 18\r\nConnection: close\r\n\r\ninvalid parameters".to_vec();
        let (t, seen, handle) = server(vec![rejection], true);
        assert!(submit_with(prepared("video-generation", "rejected"), Some(&t)).is_err());
        handle.join().unwrap();
        assert_eq!(
            submit_with(prepared("video-generation", "rejected"), Some(&t)).unwrap()["status"],
            "rejected"
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
    #[test]
    fn video_job_resume_and_redirect_auth() {
        if std::env::var_os("SKYWRITE_GENERATION_VIDEO_CHILD").is_none() {
            let temp = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "generation::tests::video_job_resume_and_redirect_auth",
                    "--nocapture",
                ])
                .env("SKYWRITE_GENERATION_VIDEO_CHILD", "1")
                .env("SKYWRITE_HOME", temp.path())
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let fixture = tempfile::Builder::new().suffix(".mp4").tempfile().unwrap();
        let mut args = crate::process::args(&[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=orange:s=64x64:r=10:d=0.3",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ]);
        args.push(fixture.path().as_os_str().to_owned());
        crate::process::run("ffmpeg", &args, Duration::from_secs(15), 1024 * 1024).unwrap();
        let video = fs::read(fixture.path()).unwrap();
        let content=[format!("HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",video.len()).into_bytes(),video].concat();
        let(t,seen,handle)=server(vec![json_http(json!({"id":"known-job_1","status":"pending","polling_url":"https://ignored.invalid/path"})),json_http(json!({"id":"known-job_1","status":"in_progress"})),json_http(json!({"id":"known-job_1","status":"completed","unsigned_urls":["https://ignored.invalid/path"],"usage":{"cost":0.25}})),b"HTTP/1.1 302 Found\r\nLocation: /download\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),content],true);
        let first = submit_with(prepared("video-generation", "video-resume"), Some(&t)).unwrap();
        assert_eq!(first["status"], "in_progress");
        let done = submit_with(prepared("video-generation", "video-resume"), Some(&t)).unwrap();
        assert_eq!(done["status"], "completed");
        handle.join().unwrap();
        let cached = submit_with(prepared("video-generation", "video-resume"), Some(&t)).unwrap();
        assert_eq!(cached["artifact"], done["artifact"]);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.iter().filter(|s| s.starts_with("POST")).count(), 1);
        assert_eq!(seen.len(), 5);
        assert!(seen[3].contains("/videos/known-job_1/content?index=0"));
        let media =
            crate::media::probe(Path::new(done["artifact_path"].as_str().unwrap())).unwrap();
        assert_eq!(media.video().unwrap().width, Some(64));
        assert!((media.duration - 0.3).abs() < 0.03);
    }
    #[test]
    fn signed_upscale_url_expiry_preserves_identity() {
        let mut first = prepared("video-upscaling", "upscale");
        first.common.dry_run = true;
        first.references =
            vec![json!({"sha256":"stable-source-content-hash","source_job":"known"})];
        first.body["input_references"] = json!([{"type":"video_url","video_url":{"url":"https://example.com/video?token=first"}}]);
        let a = submit_with(first, None).unwrap();
        let mut second = prepared("video-upscaling", "upscale");
        second.common.dry_run = true;
        second.references =
            vec![json!({"sha256":"stable-source-content-hash","source_job":"known"})];
        second.body["input_references"] = json!([{"type":"video_url","video_url":{"url":"https://example.com/video?token=second"}}]);
        let b = submit_with(second, None).unwrap();
        assert_eq!(a["id"], b["id"]);
        assert!(!serde_json::to_string(&a).unwrap().contains("token="));
    }
    #[test]
    fn generation_http_header_and_body_deadlines() {
        for body_stall in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let thread = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut buffer = [0; 4096];
                let _ = stream.read(&mut buffer).unwrap();
                if body_stall {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{",
                        )
                        .unwrap();
                }
                std::thread::sleep(Duration::from_millis(180));
                let _ = stream.write_all(if body_stall {
                    b"}"
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                });
            });
            let transport = Transport {
                client: Transport::client_with_deadline(Duration::from_millis(80)).unwrap(),
                key: "DUMMY".into(),
                base: format!("http://{address}"),
            };
            let start = Instant::now();
            let error = transport.json("/videos/known", None).unwrap_err();
            assert!(
                format!("{error:#}").to_lowercase().contains("timed out"),
                "{error:#}"
            );
            assert!(start.elapsed() < Duration::from_secs(1));
            thread.join().unwrap();
        }
    }
    #[test]
    fn undecodable_video_remains_ready_and_retry_only_downloads() {
        if std::env::var_os("SKYWRITE_VIDEO_DECODE_TEST_CHILD").is_none() {
            let temp = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "generation::tests::undecodable_video_remains_ready_and_retry_only_downloads",
                    "--nocapture",
                ])
                .env("SKYWRITE_VIDEO_DECODE_TEST_CHILD", "1")
                .env("SKYWRITE_HOME", temp.path())
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.mp4");
        let mut args = crate::process::args(&[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=orange:s=32x24:r=24:d=0.25",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-movflags",
            "+faststart",
        ]);
        args.push(source.as_os_str().to_owned());
        crate::process::run("ffmpeg", &args, Duration::from_secs(15), 1024 * 1024).unwrap();
        let healthy = fs::read(&source).unwrap();
        let mut corrupt = healthy.clone();
        let mdat = corrupt.windows(4).position(|p| p == b"mdat").unwrap();
        let size = u32::from_be_bytes(corrupt[mdat - 4..mdat].try_into().unwrap()) as usize;
        corrupt[mdat + 4..mdat - 4 + size].fill(0);
        fs::write(temp.path().join("corrupt.mp4"), &corrupt).unwrap();
        assert!(
            crate::media::probe(&temp.path().join("corrupt.mp4"))
                .unwrap()
                .has_video()
        );
        let binary_response = |bytes: &[u8]| {
            [format!("HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).into_bytes(), bytes.to_vec()].concat()
        };
        let (transport, seen, handle) = server(
            vec![
                json_http(json!({"id":"decode-job","status":"completed","usage":{"cost":0.25}})),
                binary_response(&corrupt),
                binary_response(&healthy),
            ],
            true,
        );
        let error = submit_with(
            prepared("video-generation", "decode-recovery"),
            Some(&transport),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("decode validation"));
        let dir = fs::read_dir(jobs_root().unwrap())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let retained = load_job(&dir).unwrap();
        assert_eq!(retained.status, "ready");
        assert!(retained.artifact.is_none());
        assert_eq!(retained.usage.unwrap()["cost"], 0.25);
        assert!(!dir.join("result.mp4").exists());
        let done = submit_with(
            prepared("video-generation", "decode-recovery"),
            Some(&transport),
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(done["status"], "completed");
        assert_eq!(done["artifact"]["decode_validation_version"], 1);
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.iter().filter(|line| line.starts_with("POST")).count(),
            1
        );
        assert_eq!(
            seen.iter()
                .filter(|line| line.contains("/content?"))
                .count(),
            2
        );
    }
    #[test]
    fn media_bounds_and_capability_inverse() {
        assert_eq!(limited(&b"1234"[..], 4).unwrap(), b"1234");
        assert!(limited(&b"12345"[..], 4).is_err());
        assert!(remote_id("../escape").is_err());
        assert!(remote_id("gen-vid_123").is_ok());
        assert!(valid_id("../escape").is_err());
        assert!(!public_address("127.0.0.1".parse().unwrap()));
        assert!(!public_address("::ffff:127.0.0.1".parse().unwrap()));
        assert!(public_address("1.1.1.1".parse().unwrap()));
        assert!(
            validate_public_url(&url::Url::parse("http://example.com/asset").unwrap()).is_err()
        );
        assert!(
            validate_public_url(&url::Url::parse("https://user:secret@example.com/asset").unwrap())
                .is_err()
        );
        let mut body = json!({});
        let caps = json!({"supported_parameters":{"resolution":{"type":"enum","values":["1K","2K"]},"seed":{"type":"boolean"}}});
        image_parameter(&caps, &mut body, "resolution", Some(json!("2K"))).unwrap();
        assert!(image_parameter(&caps, &mut body, "resolution", Some(json!("4K"))).is_err());
        assert!(image_parameter(&caps, &mut body, "unknown", Some(json!(1))).is_err());
        assert_eq!(image_type(&fixture_png()).unwrap(), ("image/png", "png"));
        assert!(image_type(b"not an image").is_err());
        let t = Transport {
            client: Transport::client().unwrap(),
            key: "SECRET".into(),
            base: BASE.into(),
        };
        let request = json!({"input_references":[{"video_url":{"url":"https://example.com/x?token=private"}}]});
        let message = t.diagnostic("SECRET https://example.com/x?token=private", Some(&request));
        assert!(!message.contains("SECRET"));
        assert!(!message.contains("token=private"));
    }
    #[test]
    fn audio_jobs_http_recovery() {
        if std::env::var_os("SKYWRITE_AUDIO_JOB_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "generation::tests::audio_jobs_http_recovery",
                    "--nocapture",
                ])
                .env("SKYWRITE_AUDIO_JOB_CHILD", "1")
                .env("SKYWRITE_HOME", home.path())
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let wav = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        let mut args = crate::process::args(&[
            "-v",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=660:duration=0.2",
            "-acodec",
            "pcm_s16le",
        ]);
        args.push(wav.path().as_os_str().to_owned());
        crate::process::run("ffmpeg", &args, Duration::from_secs(10), 1024 * 1024).unwrap();
        let bytes = fs::read(wav.path()).unwrap();
        let raw=[format!("HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).into_bytes(),bytes.clone()].concat();
        let (transport, seen, handle) = server(vec![raw], true);
        let speech = || {
            let mut p = prepared("speech-generation", "speech-http");
            p.body = json!({"model":"test/model","input":"Hello","voice":"test","response_format":"mp3"});
            p
        };
        let first = submit_with(speech(), Some(&transport)).unwrap();
        assert_eq!(first["status"], "completed");
        handle.join().unwrap();
        assert_eq!(
            seen.lock().unwrap()[0].split_whitespace().nth(1),
            Some("/audio/speech")
        );
        let dir = jobs_root().unwrap().join(first["id"].as_str().unwrap());
        let mut job = load_job(&dir).unwrap();
        job.status = "submitting".into();
        job.artifact = None;
        save_job(&dir, &mut job).unwrap();
        fs::remove_file(dir.join("result.wav")).unwrap();
        let restored = submit_with(speech(), Some(&transport)).unwrap();
        assert_eq!(restored["status"], "completed");
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "savedresponse recovery must not repeat speech POST"
        );
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let stream = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"id":"gen-music-test","choices":[{"delta":{"audio":{"data":encoded}}}]}),
            json!({"id":"gen-music-test","choices":[],"usage":{"cost":0.04}})
        );
        let response=[format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",stream.len()).into_bytes(),stream.into_bytes()].concat();
        let (transport, seen, handle) = server(vec![response], true);
        let music = || prepared("music-generation", "music-http");
        let first = submit_with(music(), Some(&transport)).unwrap();
        assert_eq!(first["status"], "completed");
        assert_eq!(first["usage"]["cost"], 0.04);
        handle.join().unwrap();
        assert_eq!(
            seen.lock().unwrap()[0].split_whitespace().nth(1),
            Some("/chat/completions")
        );
        assert_eq!(
            submit_with(music(), Some(&transport)).unwrap()["id"],
            first["id"]
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
        let partial = b"data: {\"choices\":[{\"delta\":{\"audio\":{\"data\":\"SUQz\"}}}]}\n\n";
        let response=[format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",partial.len()).into_bytes(),partial.to_vec()].concat();
        let (transport, seen, handle) = server(vec![response], true);
        let incomplete = || prepared("music-generation", "music-partial");
        assert!(submit_with(incomplete(), Some(&transport)).is_err());
        handle.join().unwrap();
        assert_eq!(
            submit_with(incomplete(), Some(&transport)).unwrap()["status"],
            "uncertain"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "incomplete audio stream must not resubmit"
        );
    }
    #[test]
    fn music_reference_blobs_are_not_persisted_or_echoed_in_errors() {
        let body = json!({"model":"google/lyria-test","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":format!("data:image/png;base64,{}","x".repeat(1024*1024))}}]}]});
        let clean = sanitized_request(&body);
        assert!(serde_json::to_vec(&clean).unwrap().len() < 1024);
        let t = Transport {
            client: Transport::client().unwrap(),
            key: "SECRET".into(),
            base: BASE.into(),
        };
        let body = json!({"messages":[{"content":[{"image_url":{"url":"data:image/png;base64,SECRETIMAGE"}}]}]});
        let error = t.diagnostic(
            "bad reference data:image/png;base64,SECRETIMAGE plus SECRET",
            Some(&body),
        );
        assert!(!error.contains("data:image"));
        assert!(!error.contains("SECRET"));
    }
}
