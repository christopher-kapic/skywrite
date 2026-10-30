//! Explicit, bounded Google Fonts CSS2 downloads for native rendering.
//!
//! Only family/axis selectors are sent. Template text is never used for subsetting.
use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use url::Url;

const CSS_ENDPOINT: &str = "https://fonts.googleapis.com/css2";
const MAX_CSS_BYTES: usize = 128 * 1024;
const MAX_FONT_BYTES: usize = 8 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_SELECTORS: usize = 8;
const MAX_FONTS: usize = 16;
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub struct LoadedFont {
    pub bytes: Vec<u8>,
    pub path: PathBuf,
    pub sha256: String,
    pub source_url: String,
}
#[derive(Debug)]
pub struct GoogleFonts {
    pub files: Vec<LoadedFont>,
    pub families: Vec<serde_json::Value>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    selector: String,
    css_url: String,
    fonts: Vec<CachedFont>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedFont {
    sha256: String,
    bytes: usize,
    source_url: String,
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Validate a CSS2 family selector, not a URL or arbitrary CSS declaration.
fn css_url(selector: &str) -> Result<Url> {
    if selector.is_empty() || selector.len() > 256 || selector.trim() != selector {
        bail!("Google font selector must contain 1..256 characters without outer whitespace");
    }
    let (family, axes) = selector
        .split_once(':')
        .map_or((selector, None), |(f, a)| (f, Some(a)));
    if family.len() > 128
        || !family.bytes().any(|b| b.is_ascii_alphabetic())
        || !family
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b' ' || b == b'-')
    {
        bail!("Google font family must use ASCII letters, numbers, spaces, or hyphens");
    }
    if let Some(axes) = axes {
        let (tags, tuples) = axes
            .split_once('@')
            .context("Google font axes require tags@values, e.g. Inter:wght@400;700")?;
        let tags: Vec<_> = tags.split(',').collect();
        if tags.is_empty()
            || tags.len() > 8
            || tags
                .iter()
                .any(|t| t.len() != 4 || !t.bytes().all(|b| b.is_ascii_alphabetic()))
            || tags.windows(2).any(|t| t[0] >= t[1])
        {
            bail!("Google font axis tags must be four ASCII letters in alphabetical order");
        }
        if tuples.is_empty() || tuples.split(';').count() > 16 {
            bail!("Google font selectors support 1..16 axis tuples");
        }
        for tuple in tuples.split(';') {
            let values: Vec<_> = tuple.split(',').collect();
            if values.len() != tags.len() {
                bail!("Google font tuple and axis counts differ");
            }
            for value in values {
                let endpoints: Vec<_> = value.split("..").collect();
                if endpoints.is_empty() || endpoints.len() > 2 {
                    bail!("invalid Google font axis range");
                }
                let mut numbers = Vec::new();
                for endpoint in endpoints {
                    if endpoint.is_empty()
                        || !endpoint
                            .bytes()
                            .all(|b| b.is_ascii_digit() || b == b'.' || b == b'-')
                    {
                        bail!("invalid Google font axis value");
                    }
                    let n: f64 = endpoint.parse().context("invalid Google font axis value")?;
                    if !n.is_finite() || n.abs() > 100_000.0 {
                        bail!("Google font axis value is out of bounds");
                    }
                    numbers.push(n);
                }
                if numbers.len() == 2 && numbers[0] >= numbers[1] {
                    bail!("Google font axis ranges must increase");
                }
            }
        }
    }
    let mut url = Url::parse(CSS_ENDPOINT)?;
    url.query_pairs_mut()
        .append_pair("family", selector)
        .append_pair("display", "swap");
    Ok(url)
}

fn font_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).context("Google Fonts returned an invalid font URL")?;
    if url.scheme() != "https"
        || url.host_str() != Some("fonts.gstatic.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().starts_with("/s/")
    {
        bail!(
            "Google Fonts asset URLs must use HTTPS fonts.gstatic.com/s/ without credentials, ports, query, or fragment"
        );
    }
    Ok(url)
}

/// CSS is metadata only; no stylesheet or CSS expressions are applied to the template.
fn font_urls(css: &str) -> Result<Vec<Url>> {
    let mut urls = Vec::new();
    let mut remainder = css;
    while let Some((_, after)) = remainder.split_once("url(") {
        let (raw, rest) = after
            .split_once(')')
            .context("malformed Google Fonts CSS URL")?;
        let raw = raw.trim();
        let raw = if raw.len() >= 2
            && ((raw.starts_with('\'') && raw.ends_with('\''))
                || (raw.starts_with('"') && raw.ends_with('"')))
        {
            &raw[1..raw.len() - 1]
        } else {
            raw
        };
        let url = font_url(raw)?;
        if !urls.contains(&url) {
            urls.push(url);
        }
        if urls.len() > MAX_FONTS {
            bail!(
                "Google Fonts response exceeds sixteen font files; request fewer weights or families"
            );
        }
        remainder = rest;
    }
    if urls.is_empty() {
        bail!("Google Fonts returned no usable font assets");
    }
    Ok(urls)
}

fn bounded_read(reader: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut data)?;
    if data.len() > limit {
        bail!("Google Fonts resource exceeds {limit} byte limit");
    }
    Ok(data)
}
fn read_cached(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening font cache {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("font cache inputs must be regular files");
    }
    bounded_read(file, limit)
}
fn client() -> Result<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        // Stable non-browser UA requests complete TTFs rather than script-subset CSS.
        .user_agent("Skywrite/0.1 GoogleFonts")
        .build()
        .context("initializing Google Fonts HTTPS client")
}
fn download(client: &Client, url: Url, limit: usize, started: Instant) -> Result<Vec<u8>> {
    let remaining = TOTAL_TIMEOUT
        .checked_sub(started.elapsed())
        .context("Google Fonts download exceeded 120 second total deadline")?;
    let response = client
        .get(url)
        .timeout(remaining.min(Duration::from_secs(60)))
        .send()
        .context("Google Fonts request failed")?;
    let status = response.status();
    if !status.is_success() {
        // Read bounded error text so a nonexistent family gets a useful diagnostic.
        let body = bounded_read(response, MAX_CSS_BYTES)?;
        let excerpt: String = String::from_utf8_lossy(&body).chars().take(500).collect();
        bail!(
            "Google Fonts HTTP {}: {} (redirects are not followed)",
            status.as_u16(),
            excerpt
        );
    }
    if response.content_length().is_some_and(|n| n > limit as u64) {
        bail!("Google Fonts resource exceeds {limit} byte limit");
    }
    bounded_read(response, limit)
}
fn cache_path(root: &Path, sha: &str) -> Result<PathBuf> {
    if sha.len() != 64
        || !sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("invalid font cache SHA-256");
    }
    Ok(root.join("files").join(format!("{sha}.font")))
}
fn cached(manifest: &Manifest, root: &Path, selector: &str, url: &Url) -> Result<Vec<LoadedFont>> {
    if manifest.version != 1
        || manifest.selector != selector
        || manifest.css_url != url.as_str()
        || manifest.fonts.is_empty()
        || manifest.fonts.len() > MAX_FONTS
    {
        bail!("font cache manifest does not match the requested family/version");
    }
    let mut files = Vec::new();
    let mut total = 0;
    for font in &manifest.fonts {
        font_url(&font.source_url)?;
        let path = cache_path(root, &font.sha256)?;
        let bytes = read_cached(&path, MAX_FONT_BYTES)?;
        total += bytes.len();
        if total > MAX_TOTAL_BYTES || font.bytes != bytes.len() || digest(&bytes) != font.sha256 {
            bail!(
                "font cache integrity check failed; remove the affected fonts/google cache entry to refetch"
            );
        }
        files.push(LoadedFont {
            bytes,
            path,
            sha256: font.sha256.clone(),
            source_url: font.source_url.clone(),
        });
    }
    Ok(files)
}

/// Load explicit CSS2 selectors, using verified cached files before any network access.
/// Caller supplies remaining resource budgets after local images/fonts were admitted.
pub fn load(
    selectors: &[String],
    cache_dir: Option<&Path>,
    offline: bool,
    max_bytes: usize,
    max_files: usize,
) -> Result<GoogleFonts> {
    if selectors.len() > MAX_SELECTORS {
        bail!("at most eight Google font selectors are supported");
    }
    if selectors.is_empty() {
        return Ok(GoogleFonts {
            files: Vec::new(),
            families: Vec::new(),
        });
    }
    let root = crate::cache::cache_root(cache_dir)?.join("fonts/google");
    let started = Instant::now();
    let mut client_instance = None;
    let mut loaded = GoogleFonts {
        files: Vec::new(),
        families: Vec::new(),
    };
    let mut total = 0;
    for selector in selectors {
        let url = css_url(selector)?;
        let manifest_path = root
            .join("families")
            .join(format!("{}.json", digest(url.as_str().as_bytes())));
        let (files, hit) = if manifest_path.try_exists()? {
            let manifest: Manifest =
                serde_json::from_slice(&read_cached(&manifest_path, MAX_MANIFEST_BYTES)?)
                    .context("invalid Google font cache manifest")?;
            (cached(&manifest, &root, selector, &url)?, true)
        } else {
            if offline {
                bail!(
                    "Google font {selector:?} is not cached; remove --offline to fetch this explicit family"
                );
            }
            let http = match client_instance.as_ref() {
                Some(client) => client,
                None => {
                    client_instance = Some(client()?);
                    client_instance.as_ref().unwrap()
                }
            };
            let css = download(http, url.clone(), MAX_CSS_BYTES, started)?;
            let urls =
                font_urls(std::str::from_utf8(&css).context("Google Fonts CSS must be UTF-8")?)?;
            if loaded.files.len()
                + urls
                    .iter()
                    .filter(|u| !loaded.files.iter().any(|f| f.source_url == u.as_str()))
                    .count()
                > max_files.min(MAX_FONTS)
            {
                bail!(
                    "Google fonts exceed remaining font file budget; request fewer weights or families"
                );
            }
            let mut files = Vec::new();
            for url in urls {
                if let Some(existing) = loaded.files.iter().find(|f| f.source_url == url.as_str()) {
                    files.push(LoadedFont {
                        bytes: existing.bytes.clone(),
                        path: existing.path.clone(),
                        sha256: existing.sha256.clone(),
                        source_url: existing.source_url.clone(),
                    });
                    continue;
                }
                let remaining = max_bytes.min(MAX_TOTAL_BYTES).saturating_sub(
                    total
                        + files
                            .iter()
                            .filter(|f| !loaded.files.iter().any(|l| l.sha256 == f.sha256))
                            .map(|f| f.bytes.len())
                            .sum::<usize>(),
                );
                if remaining == 0 {
                    bail!("Google fonts exceed remaining image resource byte budget");
                }
                let bytes = download(http, url.clone(), MAX_FONT_BYTES.min(remaining), started)?;
                if bytes.is_empty() {
                    bail!("Google Fonts returned an empty font asset");
                }
                let sha256 = digest(&bytes);
                let path = cache_path(&root, &sha256)?;
                if path.try_exists()? {
                    if digest(&read_cached(&path, MAX_FONT_BYTES)?) != sha256 {
                        bail!("font cache integrity check failed");
                    }
                } else {
                    match crate::io::atomic_write(&path, &bytes, false) {
                        Ok(()) => (),
                        Err(error) => {
                            // A concurrent writer may have published this immutable hash.
                            if !path.try_exists()?
                                || digest(&read_cached(&path, MAX_FONT_BYTES)?) != sha256
                            {
                                return Err(error).context("saving Google font cache");
                            }
                        }
                    }
                }
                files.push(LoadedFont {
                    bytes,
                    path,
                    sha256,
                    source_url: url.to_string(),
                });
            }
            let manifest = Manifest {
                version: 1,
                selector: selector.clone(),
                css_url: url.to_string(),
                fonts: files
                    .iter()
                    .map(|f| CachedFont {
                        sha256: f.sha256.clone(),
                        bytes: f.bytes.len(),
                        source_url: f.source_url.clone(),
                    })
                    .collect(),
            };
            match crate::io::atomic_write(&manifest_path, &serde_json::to_vec(&manifest)?, false) {
                Ok(()) => (),
                Err(error) => {
                    if !manifest_path.try_exists()? {
                        return Err(error).context("saving Google font family cache");
                    }
                    let published: Manifest =
                        serde_json::from_slice(&read_cached(&manifest_path, MAX_MANIFEST_BYTES)?)?;
                    files = cached(&published, &root, selector, &url)?;
                }
            }
            (files, false)
        };
        let mut hashes = Vec::new();
        for font in files {
            hashes.push(font.sha256.clone());
            if loaded
                .files
                .iter()
                .any(|existing| existing.sha256 == font.sha256)
            {
                continue;
            }
            total += font.bytes.len();
            if total > max_bytes.min(MAX_TOTAL_BYTES)
                || loaded.files.len() >= max_files.min(MAX_FONTS)
            {
                bail!(
                    "Google fonts exceed remaining image resource budget; request fewer weights or families"
                );
            }
            loaded.files.push(font);
        }
        loaded.families.push(serde_json::json!({"selector":selector,"css_url":url.as_str(),"cached":hit,"font_sha256":hashes,"license_note":"Check each family's license before distributing fonts; Google Fonts includes multiple licenses."}));
    }
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selectors_preserve_css2_axes_without_arbitrary_query_or_template_text() {
        for spec in [
            "Inter",
            "Crimson Pro",
            "Inter:wght@400;700",
            "Inter:wght@100..900",
            "Crimson Pro:ital,wght@0,400;1,700",
        ] {
            let url = css_url(spec).unwrap();
            assert_eq!(url.host_str(), Some("fonts.googleapis.com"));
            assert_eq!(
                url.query_pairs().collect::<Vec<_>>(),
                vec![
                    ("family".into(), spec.into()),
                    ("display".into(), "swap".into())
                ]
            );
        }
        for bad in [
            "",
            " Inter",
            "Inter&text=secret",
            "https://localhost/font",
            "Inter:wght@NaN",
            "Inter:wght@900..100",
            "Inter:wght@400,500",
            "Inter:wght,ital@400,1",
            "Inter:wght@400&text=private",
        ] {
            assert!(css_url(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn css_extracts_only_fixed_https_font_origin_and_bounds_resources() {
        let valid = "@font-face { src: url('https://fonts.gstatic.com/s/inter/v1/font.ttf') format('truetype'); }";
        assert_eq!(font_urls(valid).unwrap().len(), 1);
        assert_eq!(font_urls(&format!("{valid}{valid}")).unwrap().len(), 1);
        for bad in [
            "http://fonts.gstatic.com/s/font.ttf",
            "https://fonts.gstatic.com.example.com/s/font.ttf",
            "https://user@fonts.gstatic.com/s/font.ttf",
            "https://127.0.0.1/s/font.ttf",
            "file:///tmp/font.ttf",
            "https://fonts.gstatic.com/other/font.ttf",
            "https://fonts.gstatic.com/s/font.ttf?other=1",
        ] {
            assert!(font_urls(&format!("src:url({bad})")).is_err(), "{bad}");
        }
        assert!(font_urls("div {color:red}").is_err());
        assert!(font_urls("src:url(')").is_err());
        assert!(font_urls("src:url('')").is_err());
        assert!(bounded_read(&b"1234"[..], 3).is_err());
        assert_eq!(bounded_read(&b"123"[..], 3).unwrap(), b"123");
        let many = (0..17)
            .map(|i| format!("url(https://fonts.gstatic.com/s/a/{i}.ttf)"))
            .collect::<String>();
        assert!(font_urls(&many).is_err());
    }
    #[test]
    fn cached_manifest_hashes_are_verified_and_paths_are_not_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let font = include_bytes!("../assets/fonts/Geist.woff2");
        let sha = digest(font);
        let path = cache_path(dir.path(), &sha).unwrap();
        crate::io::atomic_write(&path, font, false).unwrap();
        let url = css_url("Geist").unwrap();
        let mut m = Manifest {
            version: 1,
            selector: "Geist".into(),
            css_url: url.to_string(),
            fonts: vec![CachedFont {
                sha256: sha,
                bytes: font.len(),
                source_url: "https://fonts.gstatic.com/s/geist/v1/font.woff2".into(),
            }],
        };
        assert_eq!(cached(&m, dir.path(), "Geist", &url).unwrap().len(), 1);
        m.fonts[0].sha256 = "../../outside".into();
        assert!(cached(&m, dir.path(), "Geist", &url).is_err());
        m.fonts[0].sha256 = digest(font);
        std::fs::write(&path, b"changed").unwrap();
        assert!(cached(&m, dir.path(), "Geist", &url).is_err());
    }
    #[test]
    fn offline_reuse_checks_manifest_identity_hash_and_remaining_resource_budget() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("fonts/google");
        let data = include_bytes!("../assets/fonts/Geist.woff2");
        let sha = digest(data);
        let path = cache_path(&root, &sha).unwrap();
        crate::io::atomic_write(&path, data, false).unwrap();
        let selector = "Geist".to_string();
        let url = css_url(&selector).unwrap();
        let manifest = Manifest {
            version: 1,
            selector: selector.clone(),
            css_url: url.to_string(),
            fonts: vec![CachedFont {
                sha256: sha,
                bytes: data.len(),
                source_url: "https://fonts.gstatic.com/s/geist/v1/font.woff2".into(),
            }],
        };
        let mpath = root
            .join("families")
            .join(format!("{}.json", digest(url.as_str().as_bytes())));
        crate::io::atomic_write(&mpath, &serde_json::to_vec(&manifest).unwrap(), false).unwrap();
        let loaded = load(
            std::slice::from_ref(&selector),
            Some(dir.path()),
            true,
            data.len(),
            1,
        )
        .unwrap();
        assert_eq!(loaded.files.len(), 1);
        assert_eq!(loaded.files[0].bytes, data);
        assert_eq!(loaded.families[0]["cached"], true);
        assert!(
            load(
                std::slice::from_ref(&selector),
                Some(dir.path()),
                true,
                data.len() - 1,
                1
            )
            .is_err()
        );
        assert!(
            load(
                std::slice::from_ref(&selector),
                Some(dir.path()),
                true,
                data.len(),
                0
            )
            .is_err()
        );
        assert!(
            load(
                &["Uncached".into()],
                Some(dir.path()),
                true,
                MAX_TOTAL_BYTES,
                16
            )
            .unwrap_err()
            .to_string()
            .contains("not cached")
        );
        std::fs::write(&path, b"corrupt").unwrap();
        assert!(load(&[selector], Some(dir.path()), true, MAX_TOTAL_BYTES, 16).is_err());
        // Empty selector lists work without HOME/XDG access and without initializing HTTP.
        assert!(load(&[], None, true, 0, 0).unwrap().files.is_empty());
    }

    #[test]
    fn real_transport_rejects_redirects_and_honors_total_deadline() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            },
            thread,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let server_hits = hits.clone();
        let server = thread::spawn(move || {
            for response in [
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/never\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0u8; 4096];
                let n = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..n]);
                assert!(!request.to_ascii_lowercase().contains("authorization:"));
                server_hits.fetch_add(1, Ordering::SeqCst);
                if response.starts_with("HTTP/1.1 200") {
                    thread::sleep(Duration::from_millis(300));
                }
                let _ = stream.write_all(response.as_bytes());
            }
        });
        let http = client().unwrap();
        let url = Url::parse(&format!("http://{address}/test")).unwrap();
        let error = download(&http, url.clone(), 10, Instant::now()).unwrap_err();
        assert!(error.to_string().contains("302"), "{error:#}");
        let start = Instant::now();
        let almost_expired = Instant::now()
            .checked_sub(TOTAL_TIMEOUT - Duration::from_millis(100))
            .unwrap();
        assert!(download(&http, url, 10, almost_expired).is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        // A response that sends headers promptly but stalls its body is bounded too.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let body_server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0u8; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n")
                .unwrap();
            thread::sleep(Duration::from_millis(300));
            let _ = stream.write_all(b"ok");
        });
        let start = Instant::now();
        let almost_expired = Instant::now()
            .checked_sub(TOTAL_TIMEOUT - Duration::from_millis(100))
            .unwrap();
        assert!(
            download(
                &http,
                Url::parse(&format!("http://{address}/body")).unwrap(),
                10,
                almost_expired
            )
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        body_server.join().unwrap();

        // A chunked response cannot bypass the byte limit by omitting Content-Length.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let cap_server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0u8; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\n1234\r\n0\r\n\r\n");
        });
        let error = download(
            &http,
            Url::parse(&format!("http://{address}/cap")).unwrap(),
            3,
            Instant::now(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("byte limit"), "{error:#}");
        cap_server.join().unwrap();
    }
}
