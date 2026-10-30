//! Optional Takumi adapter for title cards and other still assets.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
pub struct RenderImage<'a> {
    pub file: &'a Path,
    pub output: &'a Path,
    pub width: u32,
    pub height: u32,
    pub format: Option<&'a str>,
    pub quality: Option<u8>,
    pub fonts: &'a [String],
    pub overwrite: bool,
}
pub fn render(options: &RenderImage<'_>) -> Result<Value> {
    if !(1..=8192).contains(&options.width) || !(1..=8192).contains(&options.height) {
        bail!("image dimensions must be between 1 and 8192 pixels");
    }
    if options.quality.is_some_and(|q| q > 100) {
        bail!("image quality must be between 0 and 100");
    }
    let source = crate::media::canonical_file(options.file)?;
    if std::fs::metadata(&source)?.len() > 8 * 1024 * 1024 {
        bail!("image template exceeds eight MiB");
    }
    if options.output.exists() {
        if options.output.canonicalize()? == source {
            bail!("output would overwrite the input template");
        }
        if !options.overwrite {
            bail!("output exists; use --overwrite to replace it");
        }
    }
    let format = options
        .format
        .or_else(|| options.output.extension().and_then(|e| e.to_str()))
        .unwrap_or("png");
    let format = match format {
        "png" => "png",
        "webp" => "webp",
        "jpeg" | "jpg" => "jpeg",
        _ => bail!("image format must be png, webp, or jpeg"),
    };
    let parent = options
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temp = tempfile::tempdir_in(parent)?;
    let rendered = temp.path().join(format!("image.{format}"));
    let mut args = crate::process::args(&["--file"]);
    args.push(source.clone().into_os_string());
    args.extend(crate::process::args(&["--output"]));
    args.push(rendered.clone().into_os_string());
    args.extend(crate::process::args(&[
        "--width",
        &options.width.to_string(),
        "--height",
        &options.height.to_string(),
        "--format",
        format,
    ]));
    if let Some(quality) = options.quality {
        args.extend(crate::process::args(&["--quality", &quality.to_string()]));
    }
    for font in options.fonts {
        args.push(format!("--font={font}").into());
    }
    crate::process::run("takumi", &args, Duration::from_secs(120), 1024 * 1024)
        .context("rendering still asset with optional Takumi CLI")?;
    let metadata = crate::media::probe(&rendered)?;
    let video = metadata
        .video()
        .context("Takumi did not produce a decodable image")?;
    if video.width != Some(options.width) || video.height != Some(options.height) {
        bail!("Takumi output dimensions differ from requested dimensions");
    }
    if metadata.size_bytes > 64 * 1024 * 1024 {
        bail!("Takumi output exceeds 64 MiB");
    }
    crate::io::atomic_write(
        options.output,
        &std::fs::read(&rendered)?,
        options.overwrite,
    )?;
    let path: PathBuf = options.output.canonicalize()?;
    Ok(
        json!({"path":path,"width":options.width,"height":options.height,"format":format,"renderer":"takumi-cli","template":source}),
    )
}
