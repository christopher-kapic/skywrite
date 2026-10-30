//! Native upstream Takumi rendering.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
use takumi::prelude::{
    FontResource, Fonts, FromHtmlOptions, ImageSource, ImageSourceInput, Node, NodeKind,
    OutputFormat, Quality, RenderOptions, Viewport,
};

const MAX_TEMPLATE_BYTES: usize = 1024 * 1024;
const MAX_NODES: usize = 4096;
const MAX_DEPTH: usize = 64;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_RESOURCE_BYTES: usize = 16 * 1024 * 1024;

pub struct RenderImage<'a> {
    pub file: &'a Path,
    pub output: &'a Path,
    pub width: u32,
    pub height: u32,
    pub format: Option<&'a str>,
    pub quality: Option<u8>,
    /// Individual local font files for native rendering.
    pub fonts: &'a [String],
    pub overwrite: bool,
}

/// Open without waiting on FIFOs and enforce the byte budget while reading.
fn read_local(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut options = std::fs::OpenOptions::new();
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
        bail!("image inputs must be regular local files");
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("image input exceeds {limit} byte limit");
    }
    Ok(bytes)
}

fn protect_output(output: &Path, inputs: &[PathBuf], overwrite: bool) -> Result<()> {
    if output.exists() {
        let resolved = output.canonicalize()?;
        if inputs.contains(&resolved) {
            bail!("output would overwrite an input template, font, or image");
        }
        if !overwrite {
            bail!("output exists; use --overwrite to replace it");
        }
    }
    Ok(())
}

pub fn render(options: &RenderImage<'_>) -> Result<Value> {
    if !(1..=8192).contains(&options.width)
        || !(1..=8192).contains(&options.height)
        || u64::from(options.width) * u64::from(options.height) > MAX_PIXELS
    {
        bail!("image dimensions must be 1..8192 with at most 16777216 canvas pixels");
    }
    if options.quality.is_some_and(|q| q > 100) {
        bail!("image quality must be between 0 and 100");
    }
    let source = crate::media::canonical_file(options.file)?;
    let bytes = read_local(&source, MAX_TEMPLATE_BYTES)?;
    protect_output(
        options.output,
        std::slice::from_ref(&source),
        options.overwrite,
    )?;
    let format = match options
        .format
        .or_else(|| options.output.extension().and_then(|e| e.to_str()))
        .unwrap_or("png")
    {
        "png" => "png",
        "webp" => "webp",
        "jpeg" | "jpg" => "jpeg",
        _ => bail!("image format must be png, webp, or jpeg"),
    };
    // PNG is lossless: reject a setting that would otherwise be silently ignored.
    if format == "png" && options.quality.is_some() {
        bail!("--quality applies only to JPEG and WebP");
    }
    native(options, &source, &bytes, format)?;
    Ok(
        json!({"path":options.output.canonicalize()?,"width":options.width,"height":options.height,"format":format,"renderer":"takumi","renderer_version":"2.19.0","template":source}),
    )
}

/// Bound JSON before handing the tree to upstream's own Node deserializer.
fn validate_json(value: &Value, depth: usize, count: &mut usize) -> Result<()> {
    *count += 1;
    if depth > MAX_DEPTH || *count > MAX_NODES * 16 {
        bail!("image JSON exceeds complexity limits");
    }
    match value {
        Value::Array(values) => {
            for v in values {
                validate_json(v, depth + 1, count)?;
            }
        }
        Value::Object(values) => {
            for v in values.values() {
                validate_json(v, depth + 1, count)?;
            }
        }
        Value::Number(n)
            if n.as_f64()
                .is_none_or(|v| !v.is_finite() || v.abs() > 1_000_000.0) =>
        {
            bail!("image JSON numbers must be finite and within one million")
        }
        _ => (),
    }
    Ok(())
}

#[derive(Default)]
struct LocalResources {
    inputs: Vec<PathBuf>,
    bytes: usize,
    pixels: u64,
    images: usize,
}

fn native(options: &RenderImage<'_>, source: &Path, bytes: &[u8], format: &str) -> Result<()> {
    let mut node: Node = match source.extension().and_then(|e| e.to_str()) {
        Some("json") => {
            let value: Value = serde_json::from_slice(bytes).context("invalid image node JSON")?;
            validate_json(&value, 0, &mut 0)?;
            serde_json::from_value(value).context("invalid upstream Takumi node")?
        }
        Some("html" | "htm") => {
            let html = std::str::from_utf8(bytes).context("image HTML must be UTF-8")?;
            // Upper bound on tokenizer work/tree size before invoking the upstream parser.
            if html.bytes().filter(|b| *b == b'<').count() > MAX_NODES * 2 {
                bail!("image HTML exceeds tag limit");
            }
            // The stable upstream from_html API uses inline style and tw attributes;
            // it discards stylesheets, so reject them instead of silently dropping CSS.
            let lowercase = html.to_ascii_lowercase();
            if lowercase.contains("<style") || lowercase.contains("<link") {
                bail!(
                    "native image HTML supports inline style/tw attributes; external or <style> stylesheets are unsupported"
                );
            }
            takumi::from_html(
                html,
                FromHtmlOptions::builder().max_depth(MAX_DEPTH).build(),
            )
            .context("invalid image HTML")?
        }
        _ => bail!("native image template must have .html, .htm, or .json extension"),
    };
    let mut resources = LocalResources {
        inputs: vec![source.to_path_buf()],
        ..Default::default()
    };
    resolve_nodes(
        &mut node,
        source.parent().unwrap_or(Path::new(".")),
        0,
        &mut 0,
        &mut resources,
    )?;
    if options.fonts.len() > 16 {
        bail!("at most sixteen extra font files are supported");
    }
    let mut fonts = Fonts::default();
    fonts
        .register(FontResource::new(
            include_bytes!("../assets/fonts/Geist.woff2").as_slice(),
        ))
        .context("registering embedded Geist font")?;
    for path in options.fonts {
        let path = crate::media::canonical_file(Path::new(path))?;
        let data = read_local(&path, MAX_RESOURCE_BYTES)?;
        resources.bytes += data.len();
        if resources.bytes > 32 * 1024 * 1024 {
            bail!("image resources exceed 32 MiB total");
        }
        fonts
            .register(FontResource::new(data))
            .with_context(|| format!("registering font {}", path.display()))?;
        resources.inputs.push(path);
    }
    protect_output(options.output, &resources.inputs, options.overwrite)?;
    let bitmap = takumi::render(
        RenderOptions::builder()
            .viewport(Viewport::new((options.width, options.height)))
            .node(node)
            .fonts(&fonts)
            .build(),
    )
    .context("rendering native upstream Takumi image")?;
    let quality = options.quality.map(Quality::new).unwrap_or_default();
    let codec = match format {
        "png" => OutputFormat::Png,
        "webp" => OutputFormat::WebP { quality },
        _ => OutputFormat::Jpeg { quality },
    };
    let mut encoded = Vec::new();
    takumi::write_image(&bitmap, &mut encoded, codec).context("encoding native Takumi image")?;
    if encoded.len() > 64 * 1024 * 1024 {
        bail!("encoded image exceeds 64 MiB");
    }
    crate::io::atomic_write(options.output, &encoded, options.overwrite)
}

fn resolve_nodes(
    node: &mut Node,
    base: &Path,
    depth: usize,
    count: &mut usize,
    resources: &mut LocalResources,
) -> Result<()> {
    *count += 1;
    if depth > MAX_DEPTH || *count > MAX_NODES {
        bail!("image node tree exceeds depth or node limit");
    }
    match &mut node.kind {
        NodeKind::Container { children } => {
            for child in children {
                resolve_nodes(child, base, depth + 1, count, resources)?;
            }
        }
        NodeKind::Image(image) => {
            if [image.width, image.height]
                .into_iter()
                .flatten()
                .any(|v| !v.is_finite() || v <= 0.0 || v > 8192.0)
            {
                bail!("image node dimensions must be finite and in 0..8192");
            }
            let ImageSourceInput::Url(src) = &image.src else {
                bail!("native image nodes require a local file path in src");
            };
            if src.contains(':') || src.starts_with("//") {
                bail!(
                    "native image sources must be local paths; URLs and data URIs are unsupported"
                );
            }
            if resources.images >= 64 {
                bail!("too many image resources");
            }
            let path = crate::media::canonical_file(&base.join(src.as_ref()))?;
            let data = read_local(&path, MAX_RESOURCE_BYTES)?;
            resources.bytes += data.len();
            if resources.bytes > 32 * 1024 * 1024 {
                bail!("image resources exceed 32 MiB total");
            }
            let dimensions = imagesize::blob_size(&data)
                .context("reading local raster image dimensions (PNG/JPEG/WebP)")?;
            let pixels = (dimensions.width as u64)
                .checked_mul(dimensions.height as u64)
                .context("local image dimensions overflow")?;
            if dimensions.width == 0 || dimensions.height == 0 || pixels > MAX_PIXELS {
                bail!("local image exceeds pixel limit");
            }
            resources.pixels += pixels;
            resources.images += 1;
            if resources.pixels > MAX_PIXELS * 2 {
                bail!("local images exceed 32 megapixels combined");
            }
            image.src = ImageSourceInput::Loaded(
                ImageSource::from_bytes(&data).context("decoding local image")?,
            );
            resources.inputs.push(path);
        }
        NodeKind::Text(_) => (),
        _ => bail!("unsupported upstream node kind"),
    }
    Ok(())
}
