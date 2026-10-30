//! Explicit S3/R2 uploads. Live bucket compatibility is intentionally user-unverified.
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use clap::Subcommand;
use futures_util::{StreamExt, TryStreamExt};
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::client::{
    ClientOptions, HttpClient, HttpConnector, HttpError, HttpErrorKind, HttpRequest, HttpResponse,
    HttpService,
};
use object_store::{
    Attribute, Attributes, GetOptions, ObjectStore, ObjectStoreExt, PutMode, PutOptions,
    RetryConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use url::Url;
pub const MAX_UPLOAD_BYTES: u64 = 512 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const CONFIG_LIMIT: u64 = 64 * 1024;
const NAMESPACE: &str = "skywrite/uploads/";
const OUTPUTS_NAMESPACE: &str = "skywrite/outputs/";
fn default_outputs_prefix() -> String {
    OUTPUTS_NAMESPACE.into()
}
#[derive(Debug, Subcommand)]
pub enum StorageCommand {
    /// Save non-secret settings. Never creates a bucket or changes its ACL.
    Configure {
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        bucket: String,
        #[arg(long)]
        public_base_url: String,
        #[arg(long, default_value = "auto")]
        region: String,
        #[arg(long, default_value = NAMESPACE)]
        prefix: String,
        #[arg(long, default_value = OUTPUTS_NAMESPACE)]
        outputs_prefix: String,
    },
    /// Read non-secret settings without contacting S3 or reading credentials.
    Status,
    /// Reuse or upload a content-addressed MP4. Its public availability is intentional.
    Upload {
        input: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete only marked Skywrite hash objects beneath the configured prefix.
    Purge {
        #[arg(long)]
        dry_run: bool,
        /// Also delete sources of unfinished local jobs; may break those jobs.
        #[arg(long)]
        include_active: bool,
        /// Maximum objects examined per sweep (1..10000); follow next_after to continue.
        #[arg(long, default_value_t = 10000)]
        limit: usize,
        /// Continue after the prior result's next_after key.
        #[arg(long)]
        after: Option<String>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub version: u32,
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub public_base_url: String,
    pub prefix: String,
    #[serde(default = "default_outputs_prefix")]
    pub outputs_prefix: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadRecord {
    pub config: StorageConfig,
    pub key: String,
    pub sha256: String,
    pub bytes: u64,
    pub media_type: String,
}
pub struct UploadPlan {
    pub record: UploadRecord,
    pub source: PathBuf,
    staged: File,
}
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn https_url(raw: &str, api: bool) -> Result<Url> {
    ensure!(raw.len() <= 4096, "storage URL exceeds 4096 bytes");
    let url = Url::parse(raw).context("invalid storage HTTPS URL")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.port_or_known_default() == Some(443),
        "storage URLs must use HTTPS without credentials, query, fragment, or a nonstandard port"
    );
    if api {
        ensure!(
            url.path() == "/",
            "S3 endpoint must be an origin without a bucket/path; configure --bucket separately"
        );
    }
    Ok(url)
}
impl StorageConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported storage configuration version"
        );
        https_url(&self.endpoint, true)?;
        https_url(&self.public_base_url, false)?;
        ensure!(
            (3..=63).contains(&self.bucket.len())
                && self
                    .bucket
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && self
                    .bucket
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && self
                    .bucket
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric),
            "bucket must contain 3..63 lowercase letters, digits or hyphens (dotted names are unsupported)"
        );
        ensure!(
            !self.region.is_empty()
                && self.region.len() <= 64
                && self
                    .region
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid region"
        );
        ensure!(
            self.prefix.starts_with(NAMESPACE)
                && self.prefix.ends_with('/')
                && self.prefix.len() <= 256,
            "prefix must be skywrite/uploads/ or a project subfolder, ending in /"
        );
        ensure!(
            self.prefix
                .split('/')
                .filter(|s| !s.is_empty())
                .all(|s| s != "."
                    && s != ".."
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
                && !self.prefix.contains("//"),
            "invalid storage prefix"
        );
        ensure!(
            self.outputs_prefix.starts_with(OUTPUTS_NAMESPACE)
                && self.outputs_prefix.ends_with('/')
                && self.outputs_prefix.len() <= 256
                && !self.outputs_prefix.contains("//")
                && self
                    .outputs_prefix
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .all(|s| s != "."
                        && s != ".."
                        && s.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')),
            "outputs prefix must be skywrite/outputs/ or a project subfolder ending in /"
        );
        Ok(())
    }
    fn same_upload_settings(&self, other: &Self) -> bool {
        self.version == other.version
            && self.endpoint == other.endpoint
            && self.bucket == other.bucket
            && self.region == other.region
            && self.public_base_url == other.public_base_url
            && self.prefix == other.prefix
    }
    pub fn public_url(&self, key: &str) -> Result<String> {
        ensure!(
            owned_hash(&self.prefix, key).is_some(),
            "object key is outside this Skywrite upload prefix"
        );
        let mut base = https_url(&self.public_base_url, false)?;
        {
            let mut segments = base
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("invalid public URL base"))?;
            segments.pop_if_empty();
            for piece in key.split('/') {
                segments.push(piece);
            }
        }
        Ok(base.to_string())
    }
    pub fn fingerprint(&self) -> Result<String> {
        Ok(digest(&serde_json::to_vec(self)?))
    }
}
pub fn config_path() -> Result<PathBuf> {
    Ok(crate::workspace::personal_home()?.join("storage.json"))
}
fn regular_read(path: &Path, cap: u64) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "storage input must be a regular non-symlink file"
    );
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= cap,
        "storage file exceeds {cap} bytes"
    );
    Ok(bytes)
}
pub fn configuration() -> Result<StorageConfig> {
    let config: StorageConfig = serde_json::from_slice(
        &regular_read(&config_path()?, CONFIG_LIMIT)
            .context("configure a bucket with skywrite storage configure first")?,
    )?;
    config.validate()?;
    Ok(config)
}
/// Lock order: caller's job lock, then this lock. Scans never take job locks.
pub fn operation_lock() -> Result<File> {
    let home = crate::workspace::personal_home()?;
    fs::create_dir_all(&home)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .mode(0o600);
    }
    let file = options.open(home.join(".storage.lock"))?;
    ensure!(file.metadata()?.is_file(), "storage lock must be regular");
    file.lock()?;
    Ok(file)
}
fn owned_hash<'a>(prefix: &str, key: &'a str) -> Option<&'a str> {
    if key.len() > 1024 {
        return None;
    }
    let name = key.strip_prefix(prefix)?;
    let (folders, leaf) = name
        .rsplit_once('/')
        .map_or(("", name), |(folders, leaf)| (folders, leaf));
    if !folders.is_empty()
        && folders.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    {
        return None;
    }
    let hash = leaf.strip_suffix(".mp4")?;
    (hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(hash)
}
impl UploadRecord {
    pub fn validate(&self) -> Result<()> {
        self.config.validate()?;
        ensure!(
            owned_hash(&self.config.prefix, &self.key) == Some(self.sha256.as_str()),
            "invalid content-addressed upload key"
        );
        ensure!(
            self.bytes > 0 && self.bytes <= MAX_UPLOAD_BYTES && self.media_type == "video/mp4",
            "invalid upload media metadata"
        );
        Ok(())
    }
    pub fn public_url(&self) -> Result<String> {
        self.validate()?;
        self.config.public_url(&self.key)
    }
    pub fn identity(&self) -> Result<String> {
        // Prefix and CDN changes do not change the S3 object being referenced.
        Ok(digest(&serde_json::to_vec(&(
            https_url(&self.config.endpoint, true)?
                .origin()
                .ascii_serialization(),
            &self.config.bucket,
            &self.key,
        ))?))
    }
}
pub fn plan_upload(input: &Path) -> Result<UploadPlan> {
    let config = configuration()?;
    let source = crate::media::canonical_file(input)?;
    let info = crate::media::probe(&source)?;
    ensure!(
        info.has_video()
            && info.format_name.split(',').any(|name| name == "mp4")
            && info.duration.is_finite()
            && info.duration > 0.0,
        "upload source must be a playable MP4 video with a known positive duration"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let mut file = options.open(&source)?;
    ensure!(file.metadata()?.is_file(), "upload source must be regular");
    ensure!(
        file.metadata()?.len() <= MAX_UPLOAD_BYTES,
        "upload exceeds 512 MiB"
    );
    let mut staged = tempfile::tempfile()?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut head = Vec::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        ensure!(total <= MAX_UPLOAD_BYTES, "upload exceeds 512 MiB");
        if head.len() < 16 {
            head.extend_from_slice(&buffer[..n.min(16 - head.len())]);
        }
        hash.update(&buffer[..n]);
        staged.write_all(&buffer[..n])?;
    }
    ensure!(
        head.len() >= 12 && &head[4..8] == b"ftyp",
        "S3 video upload accepts MP4 only; transcode other containers with FFmpeg first"
    );
    staged.sync_all()?;
    let sha256 = hash
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let record = UploadRecord {
        key: format!("{}{sha256}.mp4", config.prefix),
        config,
        sha256,
        bytes: total,
        media_type: "video/mp4".into(),
    };
    record.validate()?;
    Ok(UploadPlan {
        record,
        source,
        staged,
    })
}
impl UploadPlan {
    pub fn protect_output(&self, output: Option<&Path>) -> Result<()> {
        if let Some(output) = output {
            crate::io::protect_inputs(output, &[self.source.clone(), config_path()?])?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone)]
struct BoundedConnector {
    client: HttpClient,
}
impl HttpConnector for BoundedConnector {
    fn connect(&self, _: &ClientOptions) -> object_store::Result<HttpClient> {
        Ok(self.client.clone())
    }
}
#[derive(Debug)]
struct BoundedService {
    inner: HttpClient,
    origin: String,
}
#[async_trait]
impl HttpService for BoundedService {
    async fn call(&self, req: HttpRequest) -> std::result::Result<HttpResponse, HttpError> {
        let url = Url::parse(&req.uri().to_string())
            .map_err(|e| HttpError::new(HttpErrorKind::Request, e))?;
        if url.origin().ascii_serialization() != self.origin {
            return Err(HttpError::new(
                HttpErrorKind::Request,
                std::io::Error::other("S3 transport refused an unexpected origin"),
            ));
        }
        let response = self.inner.execute(req).await?;
        let (parts, body) = response.into_parts();
        let mut stream = body.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(HttpError::new(
                    HttpErrorKind::Decode,
                    std::io::Error::other("S3 metadata response exceeds eight MiB"),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(HttpResponse::from_parts(parts, bytes.into()))
    }
}
pub(crate) struct Cloud {
    pub(crate) store: AmazonS3,
    pub(crate) runtime: tokio::runtime::Runtime,
}
impl Cloud {
    pub(crate) fn new(config: &StorageConfig) -> Result<Self> {
        config.validate()?;
        // Explicit keys only: no IMDS, profile, web-identity or container discovery.
        let access = std::env::var("AWS_ACCESS_KEY_ID")
            .context("set AWS_ACCESS_KEY_ID for S3/R2 operations")?;
        let secret = std::env::var("AWS_SECRET_ACCESS_KEY")
            .context("set AWS_SECRET_ACCESS_KEY for S3/R2 operations")?;
        ensure!(
            !access.is_empty()
                && access.len() <= 256
                && access.bytes().all(|b| b.is_ascii_graphic())
                && !secret.is_empty()
                && secret.len() <= 4096
                && secret.bytes().all(|b| b.is_ascii_graphic()),
            "S3 credentials must be bounded printable ASCII without whitespace"
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let store = {
            let _entered = runtime.enter();
            let client = reqwest::Client::builder()
                .https_only(true)
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(300))
                .build()?;
            let http = HttpClient::new(BoundedService {
                inner: HttpClient::new(client),
                origin: https_url(&config.endpoint, true)?
                    .origin()
                    .ascii_serialization(),
            });
            let mut builder = AmazonS3Builder::new()
                .with_bucket_name(&config.bucket)
                .with_endpoint(&config.endpoint)
                .with_region(&config.region)
                .with_access_key_id(access)
                .with_secret_access_key(secret)
                .with_virtual_hosted_style_request(false)
                .with_http_connector(BoundedConnector { client: http })
                .with_retry(RetryConfig {
                    max_retries: 2,
                    retry_timeout: Duration::from_secs(300),
                    ..Default::default()
                });
            if let Ok(token) = std::env::var("AWS_SESSION_TOKEN") {
                ensure!(
                    !token.is_empty()
                        && token.len() <= 16384
                        && token.bytes().all(|b| b.is_ascii_graphic()),
                    "AWS_SESSION_TOKEN must be bounded printable ASCII without whitespace"
                );
                builder = builder.with_token(token);
            }
            builder.build()?
        };
        Ok(Self { store, runtime })
    }
    pub(crate) fn run<T>(&self, future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        self.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(600), future)
                .await
                .context("S3 operation deadline exceeded; some effects may already be committed, and the in-flight outcome may be unknown. Retrying is idempotent; inspect or rerun the scoped operation.")?
        })
    }
    async fn marked_head(&self, record: &UploadRecord) -> Result<bool> {
        match self
            .store
            .get_opts(
                &object_store::path::Path::from(record.key.clone()),
                GetOptions::default().with_head(true),
            )
            .await
        {
            Ok(result) => {
                ensure!(
                    result.meta.size == record.bytes,
                    "existing hash object has a different size; refusing replacement"
                );
                ensure!(
                    result
                        .attributes
                        .get(&Attribute::Metadata("skywrite".into()))
                        .map(|v| v.as_ref())
                        == Some("1")
                        && result
                            .attributes
                            .get(&Attribute::Metadata("sha256".into()))
                            .map(|v| v.as_ref())
                            == Some(record.sha256.as_str()),
                    "existing hash object lacks matching Skywrite SHA metadata; refusing replacement"
                );
                Ok(true)
            }
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(error) => Err(error).context("S3 HEAD failed (only NotFound permits upload)"),
        }
    }
}
/// Caller holds the operation lock; only the prepared job path calls this.
pub fn ensure_uploaded(plan: &UploadPlan) -> Result<bool> {
    plan.record.validate()?;
    ensure!(
        configuration()?.same_upload_settings(&plan.record.config),
        "storage configuration changed; prepare this upload again"
    );
    let cloud = Cloud::new(&plan.record.config)?;
    cloud.run(async {
        if cloud.marked_head(&plan.record).await? {
            return Ok(false);
        }
        let mut staged = plan.staged.try_clone()?;
        staged.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        staged.take(MAX_UPLOAD_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == plan.record.bytes && digest(&bytes) == plan.record.sha256,
            "upload staging integrity check failed"
        );
        let mut attributes = Attributes::new();
        attributes.insert(
            Attribute::ContentType,
            plan.record.media_type.clone().into(),
        );
        attributes.insert(Attribute::Metadata("skywrite".into()), "1".into());
        attributes.insert(
            Attribute::Metadata("sha256".into()),
            plan.record.sha256.clone().into(),
        );
        let created = match cloud
            .store
            .put_opts(
                &object_store::path::Path::from(plan.record.key.clone()),
                bytes.into(),
                PutOptions {
                    mode: PutMode::Create,
                    attributes,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => true,
            Err(
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. },
            ) => false,
            Err(e) => return Err(e).context("S3 upload failed"),
        };
        ensure!(
            cloud.marked_head(&plan.record).await?,
            "uploaded object is missing after PUT"
        );
        Ok(created)
    })
}
/// Caller holds operation lock. Delete idempotently after validating bucket and marker.
pub fn delete_upload(record: &UploadRecord) -> Result<()> {
    record.validate()?;
    ensure!(
        configuration()?.same_upload_settings(&record.config),
        "storage configuration changed; restore the recorded settings before cleanup"
    );
    let cloud = Cloud::new(&record.config)?;
    cloud.run(async {
        if cloud.marked_head(record).await? {
            cloud
                .store
                .delete(&object_store::path::Path::from(record.key.clone()))
                .await?;
        }
        Ok(())
    })
}
pub fn execute(command: StorageCommand) -> Result<Value> {
    match command {
        StorageCommand::Configure {
            endpoint,
            bucket,
            public_base_url,
            region,
            prefix,
            outputs_prefix,
        } => {
            let config = StorageConfig {
                version: 1,
                endpoint,
                bucket,
                region,
                public_base_url,
                prefix,
                outputs_prefix,
            };
            config.validate()?;
            let _lock = operation_lock()?;
            let path = config_path()?;
            crate::io::write_json(&path, &config, true)?;
            #[cfg(unix)]
            File::open(path.parent().context("config parent")?)?.sync_all()?;
            Ok(
                json!({"configured":true,"config":config,"path":path,"remote_check_performed":false,"note":"No bucket contacted. Configure public access separately; credentials remain in environment variables."}),
            )
        }
        StorageCommand::Status => {
            let path = config_path()?;
            if !path.exists() {
                return Ok(json!({"configured":false,"remote_check_performed":false}));
            }
            Ok(
                json!({"configured":true,"config":configuration()?,"path":path,"remote_check_performed":false}),
            )
        }
        StorageCommand::Upload { input, dry_run } => {
            let plan = plan_upload(&input)?;
            if dry_run {
                return Ok(
                    json!({"dry_run":true,"upload":plan.record,"public_url":plan.record.public_url()?,"cloud_calls":false}),
                );
            }
            let _lock = operation_lock()?;
            let uploaded = ensure_uploaded(&plan)?;
            Ok(
                json!({"uploaded":uploaded,"reused":!uploaded,"upload":plan.record,"public_url":plan.record.public_url()?}),
            )
        }
        StorageCommand::Purge {
            dry_run,
            include_active,
            limit,
            after,
        } => purge(dry_run, include_active, limit, after.as_deref()),
    }
}
fn purge(dry_run: bool, include_active: bool, limit: usize, after: Option<&str>) -> Result<Value> {
    ensure!((1..=10000).contains(&limit), "purge limit must be 1..10000");
    let _lock = operation_lock()?;
    let config = configuration()?;
    let active = if include_active {
        std::collections::HashSet::new()
    } else {
        crate::generation::active_storage_uploads(None)?
    };
    if let Some(after) = after {
        ensure!(
            after.starts_with(&config.prefix)
                && after.len() <= 1024
                && !after.chars().any(char::is_control),
            "purge cursor must be a bounded key beneath the configured prefix"
        );
    }
    let cloud = Cloud::new(&config)?;
    cloud.run(async {
        let prefix=object_store::path::Path::from(config.prefix.trim_end_matches('/'));
        let mut listing=if let Some(after)=after { cloud.store.list_with_offset(Some(&prefix),&object_store::path::Path::from(after)) } else { cloud.store.list(Some(&prefix)) };
        let mut examined=0usize; let mut selected=Vec::new(); let mut skipped=0usize; let mut deleted=0usize; let mut truncated=false;
        let mut last_key=None; let mut inspection_errors=Vec::new(); let mut error_count=0usize;
        while let Some(meta)=listing.try_next().await.context(format!("S3 listing failed after {deleted} deletions"))? {
            if examined>=limit {truncated=true;break;} examined+=1;
            let key=meta.location.to_string(); last_key=Some(key.clone()); let Some(hash)=owned_hash(&config.prefix,&key) else {skipped+=1;continue;};
            let record=UploadRecord{config:config.clone(),key:key.clone(),sha256:hash.into(),bytes:meta.size,media_type:"video/mp4".into()};
            if record.validate().is_err() || active.contains(&record.identity()?) {skipped+=1;continue;}
            match cloud.marked_head(&record).await { Ok(true)=>{}, Ok(false)=>continue, Err(error)=>{skipped+=1;error_count+=1;
                if inspection_errors.len()<100 { inspection_errors.push(json!({"key":key,"message":format!("{error:#}").chars().take(512).collect::<String>()})); } continue;} }
            if !dry_run { cloud.store.delete(&meta.location).await.with_context(||format!("purge stopped after {deleted} deletions; failed key {key}"))?; deleted+=1; }
            selected.push(key);
        }
        Ok(json!({"dry_run":dry_run,"bucket":config.bucket,"prefix":config.prefix,"examined":examined,"deleted":deleted,"selected":selected,"skipped":skipped,"truncated":truncated,"next_after":if truncated {last_key} else {None},"inspection_error_count":error_count,"inspection_errors":inspection_errors,"complete":!truncated && error_count==0,
            "note":"Only marked Skywrite hash objects are selected. Active jobs on other machines are not visible; use separate project prefixes when sharing a bucket."}))
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> StorageConfig {
        StorageConfig {
            version: 1,
            endpoint: "https://example.r2.cloudflarestorage.com".into(),
            bucket: "test-bucket".into(),
            region: "auto".into(),
            public_base_url: "https://assets.example.com/media/".into(),
            prefix: NAMESPACE.into(),
            outputs_prefix: default_outputs_prefix(),
        }
    }
    #[test]
    fn configuration_and_key_scope() {
        let mut c = config();
        c.validate().unwrap();
        let hash = "a".repeat(64);
        let key = format!("{}{hash}.mp4", c.prefix);
        assert_eq!(
            c.public_url(&key).unwrap(),
            format!("https://assets.example.com/media/skywrite/uploads/{hash}.mp4")
        );
        for bad in [
            "",
            "/",
            "uploads/",
            "skywrite/uploads/../",
            "skywrite/uploads//",
            "skywrite/uploads/x%2fy/",
        ] {
            c.prefix = bad.into();
            assert!(c.validate().is_err(), "{bad}");
        }
        assert!(owned_hash(NAMESPACE, &format!("skywrite/uploads-extra/{hash}.mp4")).is_none());
        assert_eq!(
            owned_hash(NAMESPACE, &format!("skywrite/uploads/project/{hash}.mp4")),
            Some(hash.as_str())
        );
        assert!(owned_hash(NAMESPACE, &format!("skywrite/uploads/{hash}.txt")).is_none());
    }
    #[test]
    fn credentials_are_not_configuration_and_urls_are_unambiguous() {
        let c = config();
        let value = serde_json::to_value(&c).unwrap();
        assert!(value.get("access_key_id").is_none());
        for url in [
            "http://example.com",
            "https://user:secret@example.com",
            "https://example.com?token=x",
            "https://example.com:8443",
        ] {
            let mut c = config();
            c.endpoint = url.into();
            assert!(c.validate().is_err());
        }
        let mut c = config();
        c.endpoint = "https://example.com/bucket".into();
        assert!(c.validate().is_err());
        let unknown = json!({"version":1,"endpoint":"https://example.com","bucket":"test-bucket","region":"auto","public_base_url":"https://cdn.example.com","prefix":NAMESPACE,"secret_access_key":"NO_SECRET"});
        assert!(serde_json::from_value::<StorageConfig>(unknown).is_err());
    }
    #[test]
    fn object_identity_survives_cdn_and_prefix_changes_but_not_bucket_changes() {
        let hash = "b".repeat(64);
        let mut root = UploadRecord {
            config: config(),
            key: format!("skywrite/uploads/project-a/{hash}.mp4"),
            sha256: hash,
            bytes: 100,
            media_type: "video/mp4".into(),
        };
        root.validate().unwrap();
        let mut project = root.clone();
        project.config.prefix = "skywrite/uploads/project-a/".into();
        project.config.public_base_url = "https://other.example.com".into();
        assert_eq!(root.identity().unwrap(), project.identity().unwrap());
        project.config.bucket = "other-bucket".into();
        assert_ne!(root.identity().unwrap(), project.identity().unwrap());
        root.bytes = MAX_UPLOAD_BYTES + 1;
        assert!(root.validate().is_err());
        root.bytes = 1;
        root.key = "skywrite/uploads/project-a/other.mp4".into();
        assert!(root.validate().is_err());
    }
}
