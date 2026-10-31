use anyhow::{Context, Result, bail};
use serde::{Serialize, de::DeserializeOwned};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;

pub const MAX_JSON_BYTES: u64 = 64 * 1024 * 1024;
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("JSON input must be a regular local file");
    }
    let mut reader = file.take(MAX_JSON_BYTES + 1);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        bail!("JSON input exceeds 64 MiB");
    }
    serde_json::from_slice(&bytes).with_context(|| format!("invalid JSON in {}", path.display()))
}
/// Admit a whole set by bytes actually read, before any caller parses it.
/// Each read is limited to the remaining allowance plus one sentinel byte.
pub fn read_files_bounded(paths: &[std::path::PathBuf], limit: usize) -> Result<Vec<Vec<u8>>> {
    let mut remaining = limit;
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        if !file.metadata()?.is_file() {
            bail!("JSON input must be a regular local file");
        }
        let mut bytes = Vec::new();
        file.take(remaining as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > remaining {
            bail!("combined JSON inputs exceed {limit}-byte limit");
        }
        remaining -= bytes.len();
        files.push(bytes);
    }
    Ok(files)
}
pub fn write_json(path: &Path, value: &impl Serialize, overwrite: bool) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes, overwrite)
}
/// Keep consumed files separate from explicitly replaceable output files.
/// Atomic replacement of a distinct hard link leaves input inode bytes intact.
pub fn protect_inputs(output: &Path, inputs: &[std::path::PathBuf]) -> Result<()> {
    if output.exists() {
        let destination = output.canonicalize()?;
        for input in inputs {
            if input.canonicalize().ok().as_ref() == Some(&destination) {
                bail!("output would overwrite an input file; choose a different output path");
            }
        }
    }
    Ok(())
}
/// Publish a complete output atomically; replacement is always explicit.
pub fn atomic_write(path: &Path, bytes: &[u8], overwrite: bool) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    if overwrite {
        temp.persist(path)
            .map_err(|e| e.error)
            .with_context(|| format!("writing {}", path.display()))?;
    } else {
        temp.persist_noclobber(path)
            .map_err(|e| e.error)
            .with_context(|| {
                format!(
                    "writing {}; output exists? use --overwrite to replace it",
                    path.display()
                )
            })?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_requires_explicit_overwrite() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("out.json");
        atomic_write(&p, b"first", false).unwrap();
        assert!(atomic_write(&p, b"second", false).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"first");
        atomic_write(&p, b"second", true).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second");
    }
}
