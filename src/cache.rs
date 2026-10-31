//! User-wide regeneratable data. Personal styles/skills/config live separately.
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// Explicit flag > app environment > XDG user cache > ~/.cache/skywrite.
/// Relative XDG paths are invalid under the XDG specification and are ignored.
pub fn cache_root(override_dir: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_dir {
        return nonempty(path.to_owned());
    }
    if let Some(path) = std::env::var_os("SKYWRITE_CACHE_DIR") {
        return nonempty(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            return Ok(path.join("skywrite"));
        }
    }
    let home =
        std::env::var_os("HOME").context("set HOME or SKYWRITE_CACHE_DIR to locate the cache")?;
    Ok(nonempty(PathBuf::from(home))?.join(".cache/skywrite"))
}
fn nonempty(path: PathBuf) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("cache directory cannot be empty");
    }
    Ok(path)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FrameManifest {
    version: u32,
    source_sha256: String,
    parameters: serde_json::Value,
    frames: Vec<CachedFrame>,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedFrame {
    time: f64,
    sha256: String,
}

/// Serialize extraction per asset, publish complete immutable directories, and
/// verify hashes on reuse. Filenames come from code, never cached paths.
pub fn sampled_frames(
    info: &crate::media::MediaInfo,
    root: &Path,
    source_sha256: &str,
    count: usize,
    start: f64,
    duration: f64,
) -> Result<(Vec<crate::media::SampleFrame>, bool)> {
    if source_sha256.len() != 64 || !source_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid source SHA-256");
    }
    if !info.has_video() {
        return Ok((Vec::new(), false));
    }
    if !(1..=32).contains(&count) {
        bail!("frame count must be between 1 and 32");
    }
    let parameters = serde_json::json!({"version":1,"frames":count,"start":start,"duration":duration,"scale":"768:768:force_original_aspect_ratio=decrease","jpeg_quality":4});
    let encoded = serde_json::to_vec(&parameters)?;
    let key = crate::openrouter::cache_key(&[b"sample-frames-v1", &encoded]);
    let parent = root.join(source_sha256).join("frames");
    std::fs::create_dir_all(&parent)?;
    let parent = parent.canonicalize()?;
    let mut open = std::fs::OpenOptions::new();
    open.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let lock = open.open(parent.join(".extract.lock"))?;
    if !lock.metadata()?.is_file() {
        bail!("frame cache lock must be a regular file");
    }
    lock.lock().context("locking shared frame cache")?;
    let destination = parent.join(key);
    if destination.exists() {
        let path = destination.join("manifest.json");
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() > 64 * 1024 {
            bail!("invalid frame manifest; remove the affected cache entry");
        }
        let manifest: FrameManifest = crate::io::read_json(&path)?;
        if manifest.version != 1
            || manifest.source_sha256 != source_sha256
            || manifest.parameters != parameters
            || manifest.frames.len() != count
        {
            bail!("frame cache parameters do not match; remove the affected cache entry");
        }
        let frames = manifest
            .frames
            .iter()
            .enumerate()
            .map(|(i, frame)| {
                let path = destination.join(format!("frame-{i:03}.jpg"));
                let metadata = std::fs::symlink_metadata(&path)?;
                if !metadata.is_file()
                    || metadata.len() > 1024 * 1024
                    || !frame.time.is_finite()
                    || (frame.time
                        - (if info.duration == 0.0 {
                            0.0
                        } else {
                            start + duration * (i as f64 + 0.5) / count as f64
                        }))
                    .abs()
                        > 1e-8
                    || crate::media::file_sha256_limited(&path, 1024 * 1024)? != frame.sha256
                {
                    bail!("frame cache integrity check failed; remove the affected cache entry");
                }
                Ok(crate::media::SampleFrame {
                    time: frame.time,
                    path,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok((frames, true));
    }
    let run = tempfile::Builder::new()
        .prefix("extract-")
        .tempdir_in(&parent)?;
    let frames = crate::media::sample_frames(info, run.path(), count, start, duration)?;
    let records = frames
        .iter()
        .map(|f| {
            if std::fs::metadata(&f.path)?.len() > 1024 * 1024 {
                bail!("sample frame exceeds one MiB");
            }
            Ok(CachedFrame {
                time: f.time,
                sha256: crate::media::file_sha256_limited(&f.path, 1024 * 1024)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let manifest = FrameManifest {
        version: 1,
        source_sha256: source_sha256.into(),
        parameters,
        frames: records,
    };
    crate::io::write_json(&run.path().join("manifest.json"), &manifest, false)?;
    std::fs::rename(run.path(), &destination)?;
    Ok((
        frames
            .into_iter()
            .enumerate()
            .map(|(i, f)| crate::media::SampleFrame {
                time: f.time,
                path: destination.join(format!("frame-{i:03}.jpg")),
            })
            .collect(),
        false,
    ))
}
