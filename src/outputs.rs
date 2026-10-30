//! Cross-machine output discovery using named records and immutable SHA-256 blobs.
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use futures_util::StreamExt;
use object_store::{
    Attribute, Attributes, GetOptions, ObjectStore, ObjectStoreExt, PutMode, PutOptions,
    UpdateVersion,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_MANIFEST: usize = 8192;
const CHUNK: u64 = 4 * 1024 * 1024;
const MAX_BYTES: u64 = crate::storage::MAX_UPLOAD_BYTES;
#[derive(Debug, Subcommand)]
pub enum OutputsCommand {
    /// Share a regular file under a friendly name; no bucket ACL changes.
    Upload {
        file: PathBuf,
        #[arg(long)]
        name: Option<String>,
        /// Explicitly replace an existing named record using its current version.
        #[arg(long)]
        overwrite: bool,
        /// Hash and preview locally without reading credentials or contacting storage.
        #[arg(long)]
        dry_run: bool,
    },
    /// Discover friendly names, checksums, sizes and creation times on this bucket.
    List {
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Continue after the previous response's next_after name.
        #[arg(long)]
        after: Option<String>,
    },
    /// Download a named output, verifying size and SHA-256 before publication.
    Download {
        name: String,
        destination: PathBuf,
        #[arg(long)]
        overwrite: bool,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct OutputRecord {
    version: u32,
    name: String,
    original_name: String,
    sha256: String,
    bytes: u64,
    media_type: String,
    blob_key: String,
    created_unix_seconds: u64,
}
struct Plan {
    record: OutputRecord,
    staged: File,
}
fn name_check(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 256
            && name.trim() == name
            && !matches!(name, "." | "..")
            && !name.contains(['/', '\\'])
            && !name.chars().any(char::is_control),
        "output name must be 1..256 UTF-8 bytes without separators, controls, or outer whitespace"
    );
    Ok(())
}
fn extension(name: &str) -> &str {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty() && e.len() <= 12 && e.bytes().all(|b| b.is_ascii_alphanumeric()))
        .unwrap_or("bin")
}
fn media_type(name: &str) -> &'static str {
    match extension(name).to_ascii_lowercase().as_str() {
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "json" => "application/json",
        "xml" | "fcpxml" => "application/xml",
        "zip" => "application/zip",
        "txt" | "md" => "text/plain",
        _ => "application/octet-stream",
    }
}
fn manifest_key(prefix: &str, name: &str) -> Result<object_store::path::Path> {
    name_check(name)?;
    object_store::path::Path::parse(format!("{prefix}names/{name}.json"))
        .context("invalid output record key")
}
fn record_check(record: &OutputRecord, prefix: &str, expected_name: &str) -> Result<()> {
    ensure!(record.version == 1, "unsupported output record version");
    name_check(&record.name)?;
    name_check(&record.original_name)?;
    ensure!(
        record.name == expected_name,
        "output record name does not match its key"
    );
    ensure!(
        record.sha256.len() == 64
            && record
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid output SHA-256"
    );
    ensure!(
        record.bytes <= MAX_BYTES,
        "output exceeds 512 MiB download limit"
    );
    ensure!(
        record.media_type.len() <= 128 && record.media_type.bytes().all(|b| b.is_ascii_graphic()),
        "invalid output content type"
    );
    let expected = format!("{prefix}blobs/{}.", record.sha256);
    let suffix = record
        .blob_key
        .strip_prefix(&expected)
        .context("output blob key does not match its namespace and SHA-256")?;
    ensure!(
        !suffix.is_empty()
            && suffix.len() <= 12
            && suffix
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
        "invalid output blob extension"
    );
    Ok(())
}
fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn prepare(file: &Path, name: Option<&str>, prefix: &str) -> Result<Plan> {
    let source = crate::media::canonical_file(file)?;
    let original_name = source
        .file_name()
        .and_then(|n| n.to_str())
        .context("output source filename must be UTF-8")?
        .to_owned();
    name_check(&original_name)?;
    let name = name.unwrap_or(&original_name).to_owned();
    name_check(&name)?;
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK);
    }
    let mut input = opts.open(&source)?;
    ensure!(
        input.metadata()?.is_file(),
        "output upload must be a regular file"
    );
    ensure!(
        input.metadata()?.len() <= MAX_BYTES,
        "output upload exceeds 512 MiB"
    );
    let mut staged = tempfile::tempfile()?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut block = [0u8; 64 * 1024];
    loop {
        let n = input.read(&mut block)?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .context("output size overflow")?;
        ensure!(total <= MAX_BYTES, "output upload exceeds 512 MiB");
        hash.update(&block[..n]);
        staged.write_all(&block[..n])?;
    }
    staged.sync_all()?;
    let sha256 = hash
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let record = OutputRecord {
        version: 1,
        name,
        original_name,
        blob_key: format!(
            "{prefix}blobs/{sha256}.{}",
            extension(&source.to_string_lossy()).to_ascii_lowercase()
        ),
        sha256,
        bytes: total,
        media_type: media_type(&source.to_string_lossy()).into(),
        created_unix_seconds: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    };
    record_check(&record, prefix, &record.name)?;
    Ok(Plan { record, staged })
}
async fn read_record(
    store: &dyn ObjectStore,
    prefix: &str,
    name: &str,
) -> Result<Option<(OutputRecord, UpdateVersion)>> {
    let key = manifest_key(prefix, name)?;
    let result = match store.get(&key).await {
        Ok(r) => r,
        Err(object_store::Error::NotFound { .. }) => return Ok(None),
        Err(e) => return Err(e).context("reading output record"),
    };
    ensure!(
        result.meta.size <= MAX_MANIFEST as u64,
        "output record exceeds eight KiB"
    );
    let version = UpdateVersion {
        e_tag: result.meta.e_tag.clone(),
        version: result.meta.version.clone(),
    };
    let mut stream = result.into_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_MANIFEST,
            "output record exceeds eight KiB"
        );
        bytes.extend_from_slice(&chunk);
    }
    let record: OutputRecord =
        serde_json::from_slice(&bytes).context("invalid output record JSON")?;
    record_check(&record, prefix, name)?;
    Ok(Some((record, version)))
}
async fn blob_present(store: &dyn ObjectStore, record: &OutputRecord) -> Result<bool> {
    match store
        .get_opts(
            &object_store::path::Path::from(record.blob_key.clone()),
            GetOptions::new().with_head(true),
        )
        .await
    {
        Ok(r) => {
            ensure!(
                r.meta.size == record.bytes,
                "existing output blob size differs; refusing replacement"
            );
            ensure!(
                r.attributes
                    .get(&Attribute::Metadata("skywrite-output".into()))
                    .map(|v| v.as_ref())
                    == Some("1")
                    && r.attributes
                        .get(&Attribute::Metadata("sha256".into()))
                        .map(|v| v.as_ref())
                        == Some(record.sha256.as_str()),
                "existing output blob lacks matching ownership/hash metadata; refusing replacement"
            );
            Ok(true)
        }
        Err(object_store::Error::NotFound { .. }) => Ok(false),
        Err(e) => Err(e).context("output blob HEAD failed"),
    }
}
async fn upload(
    store: &dyn ObjectStore,
    prefix: &str,
    plan: &Plan,
    overwrite: bool,
) -> Result<Value> {
    let existing = read_record(store, prefix, &plan.record.name).await?;
    if let Some((record, _)) = &existing {
        if record.sha256 == plan.record.sha256
            && record.bytes == plan.record.bytes
            && record.blob_key == plan.record.blob_key
        {
            if blob_present(store, record).await? {
                return Ok(json!({"uploaded":false,"reused":true,"output":record}));
            }
            ensure!(
                overwrite,
                "named output blob is missing; use --overwrite to repair it"
            );
        }
        ensure!(
            overwrite,
            "output name already exists with different content; use --overwrite to replace its record"
        );
    }
    let mut created = false;
    if !blob_present(store, &plan.record).await? {
        let mut staged = plan.staged.try_clone()?;
        staged.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        staged.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == plan.record.bytes && digest(&bytes) == plan.record.sha256,
            "staged output integrity failed"
        );
        let mut attrs = Attributes::new();
        attrs.insert(
            Attribute::ContentType,
            plan.record.media_type.clone().into(),
        );
        attrs.insert(Attribute::Metadata("skywrite-output".into()), "1".into());
        attrs.insert(
            Attribute::Metadata("sha256".into()),
            plan.record.sha256.clone().into(),
        );
        match store
            .put_opts(
                &object_store::path::Path::from(plan.record.blob_key.clone()),
                bytes.into(),
                PutOptions {
                    mode: PutMode::Create,
                    attributes: attrs,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => created = true,
            Err(
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. },
            ) => {}
            Err(e) => return Err(e).context("uploading immutable output blob"),
        }
        ensure!(
            blob_present(store, &plan.record).await?,
            "output blob missing after upload"
        );
    }
    let bytes = serde_json::to_vec(&plan.record)?;
    ensure!(
        bytes.len() <= MAX_MANIFEST,
        "output record exceeds eight KiB"
    );
    let mode = match existing {
        Some((_, version)) => {
            ensure!(
                version.e_tag.is_some() || version.version.is_some(),
                "store did not provide a version for safe output replacement"
            );
            PutMode::Update(version)
        }
        None => PutMode::Create,
    };
    let mut attrs = Attributes::new();
    attrs.insert(Attribute::ContentType, "application/json".into());
    attrs.insert(Attribute::Metadata("skywrite-output".into()), "1".into());
    match store
        .put_opts(
            &manifest_key(prefix, &plan.record.name)?,
            bytes.into(),
            PutOptions {
                mode,
                attributes: attrs,
                ..Default::default()
            },
        )
        .await
    {
        Ok(_) => {}
        Err(
            object_store::Error::AlreadyExists { .. } | object_store::Error::Precondition { .. },
        ) => {
            if let Some((winner, _)) = read_record(store, prefix, &plan.record.name).await?
                && winner.sha256 == plan.record.sha256
                && winner.bytes == plan.record.bytes
                && winner.blob_key == plan.record.blob_key
            {
                return Ok(json!({"uploaded":created,"reused":true,"output":winner}));
            }
            bail!(
                "output name changed concurrently; no named record was replaced, retry after listing outputs"
            );
        }
        Err(e) => {
            return Err(e).context("publishing named output record; blob retained for safe retry");
        }
    }
    Ok(json!({"uploaded":created,"reused":!created,"output":plan.record}))
}
async fn list(
    store: &dyn ObjectStore,
    prefix: &str,
    limit: usize,
    after: Option<&str>,
) -> Result<Value> {
    ensure!(
        (1..=1000).contains(&limit),
        "outputs list --limit must be 1..1000"
    );
    if let Some(name) = after {
        name_check(name)?;
    }
    let namespace = object_store::path::Path::from(format!("{prefix}names/"));
    let after_key = after.map(|name| manifest_key(prefix, name)).transpose()?;
    let mut stream = match after_key.as_ref() {
        Some(key) => store.list_with_offset(Some(&namespace), key),
        None => store.list(Some(&namespace)),
    };
    let mut records = Vec::new();
    let mut last = None;
    let mut examined = 0usize;
    let mut more = false;
    while let Some(item) = stream.next().await {
        let item = item?;
        if examined == limit {
            more = true;
            break;
        }
        examined += 1;
        let key = item.location.as_ref();
        let suffix = key
            .strip_prefix(&format!("{prefix}names/"))
            .context("store returned an output outside the requested namespace")?;
        let name = suffix
            .strip_suffix(".json")
            .context("unexpected file in output record namespace")?;
        name_check(name)?;
        if let Some((record, _)) = read_record(store, prefix, name).await? {
            records.push(record);
        }
        last = Some(name.to_owned());
    }
    Ok(json!({"outputs":records,"next_after":if more {last}else{None},"examined":examined}))
}
async fn download(
    store: &dyn ObjectStore,
    prefix: &str,
    name: &str,
    destination: &Path,
    overwrite: bool,
) -> Result<Value> {
    name_check(name)?;
    ensure!(
        overwrite || !destination.exists(),
        "output exists; use --overwrite to replace it"
    );
    crate::io::protect_inputs(destination, &[crate::storage::config_path()?])?;
    let (record, _) = read_record(store, prefix, name)
        .await?
        .context("named output not found")?;
    let key = object_store::path::Path::from(record.blob_key.clone());
    let head = store
        .get_opts(&key, GetOptions::new().with_head(true))
        .await?;
    ensure!(
        head.meta.size == record.bytes,
        "output blob length differs from its record"
    );
    let etag = head.meta.e_tag.clone();
    let version = head.meta.version.clone();
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).context("creating private output download")?;
    let mut hasher = Sha256::new();
    let mut offset = 0u64;
    while offset < record.bytes {
        let end = offset.saturating_add(CHUNK).min(record.bytes);
        let result = store
            .get_opts(
                &key,
                GetOptions::new()
                    .with_range(Some(offset..end))
                    .with_if_match(etag.clone())
                    .with_version(version.clone()),
            )
            .await?;
        ensure!(
            result.meta.size == record.bytes && result.range == (offset..end),
            "storage did not return the requested output byte range"
        );
        let mut stream = result.into_stream();
        let mut range_bytes = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            range_bytes = range_bytes
                .checked_add(chunk.len() as u64)
                .context("download length overflow")?;
            ensure!(
                range_bytes <= end - offset,
                "output range exceeds requested bytes"
            );
            hasher.update(&chunk);
            temp.write_all(&chunk)?;
        }
        ensure!(range_bytes == end - offset, "output range ended early");
        offset = end;
    }
    let actual = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    ensure!(
        actual == record.sha256,
        "output SHA-256 mismatch; destination was not changed"
    );
    temp.as_file().sync_all()?;
    if overwrite {
        temp.persist(destination).map_err(|e| e.error)?;
    } else {
        temp.persist_noclobber(destination).map_err(|e| e.error)?;
    }
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(json!({"downloaded":true,"output":record,"destination":destination,"sha256_verified":true}))
}
pub fn execute(command: OutputsCommand) -> Result<Value> {
    let config = crate::storage::configuration()?;
    match command {
        OutputsCommand::Upload {
            file,
            name,
            overwrite,
            dry_run,
        } => {
            let plan = prepare(&file, name.as_deref(), &config.outputs_prefix)?;
            if dry_run {
                return Ok(
                    json!({"dry_run":true,"output":plan.record,"cloud_calls":false,"remote_check_performed":false}),
                );
            }
            let _lock = crate::storage::operation_lock()?;
            ensure!(
                crate::storage::configuration()? == config,
                "storage configuration changed; retry output upload"
            );
            let cloud = crate::storage::Cloud::new(&config)?;
            cloud.run(upload(
                &cloud.store,
                &config.outputs_prefix,
                &plan,
                overwrite,
            ))
        }
        OutputsCommand::List { limit, after } => {
            ensure!(
                (1..=1000).contains(&limit),
                "outputs list --limit must be 1..1000"
            );
            if let Some(name) = after.as_deref() {
                name_check(name)?;
            }
            let cloud = crate::storage::Cloud::new(&config)?;
            cloud.run(list(
                &cloud.store,
                &config.outputs_prefix,
                limit,
                after.as_deref(),
            ))
        }
        OutputsCommand::Download {
            name,
            destination,
            overwrite,
        } => {
            name_check(&name)?;
            ensure!(
                overwrite || !destination.exists(),
                "output exists; use --overwrite to replace it"
            );
            crate::io::protect_inputs(&destination, &[crate::storage::config_path()?])?;
            let cloud = crate::storage::Cloud::new(&config)?;
            cloud.run(download(
                &cloud.store,
                &config.outputs_prefix,
                &name,
                &destination,
                overwrite,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;
    const PREFIX: &str = "skywrite/outputs/";
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
    fn input(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }
    #[test]
    fn named_discovery_download_and_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let one = prepare(
            &input(dir.path(), "first.mp4", b"example output bytes"),
            Some("explainer.mp4"),
            PREFIX,
        )
        .unwrap();
        let two = prepare(
            &input(dir.path(), "second.png", b"different output bytes"),
            Some("diagram.png"),
            PREFIX,
        )
        .unwrap();
        runtime().block_on(async {
            let a = upload(&store, PREFIX, &one, false).await.unwrap();
            assert_eq!(a["uploaded"], true);
            assert_eq!(
                upload(&store, PREFIX, &one, false).await.unwrap()["reused"],
                true
            );
            upload(&store, PREFIX, &two, false).await.unwrap();
            let first = list(&store, PREFIX, 1, None).await.unwrap();
            assert_eq!(first["outputs"].as_array().unwrap().len(), 1);
            assert_eq!(first["outputs"][0]["name"], "diagram.png");
            let second = list(&store, PREFIX, 1, first["next_after"].as_str())
                .await
                .unwrap();
            assert_eq!(second["outputs"][0]["name"], "explainer.mp4");
            assert!(second["next_after"].is_null());
            let out = dir.path().join("download.mp4");
            download(&store, PREFIX, "explainer.mp4", &out, false)
                .await
                .unwrap();
            assert_eq!(std::fs::read(&out).unwrap(), b"example output bytes");
            assert!(
                download(&store, PREFIX, "explainer.mp4", &out, false)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("exists")
            );
            let replacement = prepare(
                &input(dir.path(), "new.mp4", b"replacement"),
                Some("explainer.mp4"),
                PREFIX,
            )
            .unwrap();
            assert!(
                upload(&store, PREFIX, &replacement, false)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("different content")
            );
            upload(&store, PREFIX, &replacement, true).await.unwrap();
            download(&store, PREFIX, "explainer.mp4", &out, true)
                .await
                .unwrap();
            assert_eq!(std::fs::read(&out).unwrap(), b"replacement");
            assert!(
                store
                    .head(&object_store::path::Path::from(one.record.blob_key.clone()))
                    .await
                    .is_ok(),
                "old immutable blob remains available"
            );
            let (_, old) = read_record(&store, PREFIX, "explainer.mp4")
                .await
                .unwrap()
                .unwrap();
            store
                .put(
                    &manifest_key(PREFIX, "explainer.mp4").unwrap(),
                    serde_json::to_vec(&one.record).unwrap().into(),
                )
                .await
                .unwrap();
            assert!(
                matches!(
                    store
                        .put_opts(
                            &manifest_key(PREFIX, "explainer.mp4").unwrap(),
                            serde_json::to_vec(&replacement.record).unwrap().into(),
                            PutOptions {
                                mode: PutMode::Update(old),
                                ..Default::default()
                            }
                        )
                        .await,
                    Err(object_store::Error::Precondition { .. })
                ),
                "stale record replacement cannot overwrite a concurrent update"
            );
        });
    }
    #[test]
    fn corrupt_download_never_replaces_local_output() {
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let plan = prepare(&input(dir.path(), "data.bin", b"goodbytes"), None, PREFIX).unwrap();
        runtime().block_on(async {
            upload(&store, PREFIX, &plan, false).await.unwrap();
            store
                .put(
                    &object_store::path::Path::from(plan.record.blob_key.clone()),
                    b"badbytes!".to_vec().into(),
                )
                .await
                .unwrap();
            let destination = input(dir.path(), "existing.bin", b"keep-original");
            let before = std::fs::read_dir(dir.path()).unwrap().count();
            assert!(
                download(&store, PREFIX, "data.bin", &destination, true)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("SHA-256")
            );
            assert_eq!(std::fs::read(&destination).unwrap(), b"keep-original");
            assert_eq!(
                std::fs::read_dir(dir.path()).unwrap().count(),
                before,
                "failed temporary download removed"
            );
            let absent = dir.path().join("absent.bin");
            assert!(
                download(&store, PREFIX, "data.bin", &absent, false)
                    .await
                    .is_err()
            );
            assert!(!absent.exists());
            store
                .put(
                    &object_store::path::Path::from(plan.record.blob_key.clone()),
                    b"short".to_vec().into(),
                )
                .await
                .unwrap();
            assert!(
                download(&store, PREFIX, "data.bin", &destination, true)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("length")
            );
            assert_eq!(std::fs::read(&destination).unwrap(), b"keep-original");
        });
    }
    #[test]
    fn names_record_confinement_empty_files_and_multiple_ranges() {
        for name in [
            "",
            ".",
            "..",
            "../escape",
            "a/b",
            "a\\b",
            " a.mp4",
            "a.mp4 ",
            "a\n.mp4",
        ] {
            assert!(name_check(name).is_err(), "{name:?}");
        }
        assert!(name_check("diagram 🦊 100%.png").is_ok());
        let dir = tempfile::tempdir().unwrap();
        let store = InMemory::new();
        let empty = prepare(&input(dir.path(), "empty.txt", b""), None, PREFIX).unwrap();
        let data = vec![0x5a; CHUNK as usize + 37];
        let big = prepare(
            &input(dir.path(), "large.bin", &data),
            Some("binary.dat"),
            PREFIX,
        )
        .unwrap();
        runtime().block_on(async {
            upload(&store, PREFIX, &empty, false).await.unwrap();
            let out = dir.path().join("empty-download");
            download(&store, PREFIX, "empty.txt", &out, false)
                .await
                .unwrap();
            assert_eq!(std::fs::metadata(out).unwrap().len(), 0);
            upload(&store, PREFIX, &big, false).await.unwrap();
            let out = dir.path().join("large-download");
            download(&store, PREFIX, "binary.dat", &out, false)
                .await
                .unwrap();
            assert_eq!(std::fs::read(&out).unwrap(), data);
            let mut malformed = big.record.clone();
            malformed.blob_key = format!("skywrite/uploads/{}.bin", malformed.sha256);
            assert!(record_check(&malformed, PREFIX, "binary.dat").is_err());
            malformed = big.record.clone();
            malformed.bytes = MAX_BYTES + 1;
            assert!(record_check(&malformed, PREFIX, "binary.dat").is_err());
            malformed = big.record.clone();
            malformed.blob_key = format!("{PREFIX}blobs/{}.../else", malformed.sha256);
            assert!(record_check(&malformed, PREFIX, "binary.dat").is_err());
            assert!(list(&store, PREFIX, 0, None).await.is_err());
            assert!(list(&store, PREFIX, 1001, None).await.is_err());
        });
    }
}
