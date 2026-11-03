//! Explicit, bounded OpenRouter requests with content-addressed response caching.
use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const API_BASE: &str = "https://openrouter.ai/api/v1";
const MAX_REQUEST_BYTES: usize = 24 * 1024 * 1024;
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

fn http_client(connect_timeout: Duration, request_timeout: Duration) -> Result<Client> {
    Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("initializing HTTPS client")
}
fn bounded_response(reader: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("reading OpenRouter response")?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        bail!("OpenRouter response exceeded 8 MiB");
    }
    Ok(bytes)
}

pub struct OpenRouter {
    client: Client,
    key: Option<String>,
    cache_dir: Option<PathBuf>,
    base_url: String,
}
#[derive(Debug)]
pub struct ApiResponse {
    pub body: Value,
    pub cached: bool,
}
impl OpenRouter {
    pub fn new(cache_dir: Option<&Path>) -> Result<Self> {
        let client = http_client(CONNECT_TIMEOUT, REQUEST_TIMEOUT)?;
        let key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty());
        Ok(Self {
            client,
            key,
            cache_dir: cache_dir.map(Path::to_owned),
            base_url: API_BASE.to_owned(),
        })
    }
    #[cfg(test)]
    pub(crate) fn for_test(
        base_url: String,
        cache_dir: Option<PathBuf>,
        timeout: Duration,
    ) -> Self {
        Self {
            client: http_client(timeout, timeout).unwrap(),
            key: Some("DUMMY_TEST_ONLY".into()),
            cache_dir,
            base_url,
        }
    }
    /// Public catalog GETs never attach a key; response pagination URLs are data.
    pub fn catalog(&self, endpoint: &str) -> Result<Value> {
        if !matches!(
            endpoint,
            "models"
                | "models?output_modalities=image"
                | "models?output_modalities=video"
                | "models?output_modalities=transcription"
                | "models?output_modalities=speech"
                | "models?output_modalities=audio"
                | "images/models"
                | "videos/models"
        ) {
            bail!("unsupported OpenRouter catalog endpoint");
        }
        let response = self
            .client
            .get(format!("{}/{endpoint}", self.base_url))
            .send()
            .context("OpenRouter public catalog request failed")?;
        let status = response.status();
        let bytes = bounded_response(response)?;
        if !status.is_success() {
            bail!(
                "OpenRouter catalog HTTP {}: {}",
                status.as_u16(),
                String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(2048)
                    .collect::<String>()
            );
        }
        let body: Value =
            serde_json::from_slice(&bytes).context("invalid OpenRouter catalog JSON")?;
        if !body["data"].is_array() {
            bail!("OpenRouter catalog has no data array");
        }
        Ok(body)
    }

    pub fn request(&self, endpoint: &str, body: &Value) -> Result<ApiResponse> {
        if !matches!(endpoint, "audio/transcriptions" | "chat/completions") {
            bail!("unsupported OpenRouter endpoint");
        }
        // Routing policy is not part of the content: keep it out of the cache key
        // so cached results survive policy changes and are never paid for twice.
        let encoded = serde_json::to_vec(body)?;
        let mut routed = body.clone();
        if let Some(provider) = data_policy()? {
            routed["provider"] = provider;
        }
        routed["usage"] = serde_json::json!({"include": true});
        let sent = serde_json::to_vec(&routed)?;
        if encoded.len() > MAX_REQUEST_BYTES {
            bail!(
                "OpenRouter request exceeds 24 MiB; shorten the selected range or reduce frame count"
            );
        }
        let key = cache_key(&[self.base_url.as_bytes(), endpoint.as_bytes(), &encoded]);
        let cache = self
            .cache_dir
            .as_ref()
            .map(|dir| dir.join(format!("{key}.json")));
        if let Some(path) = cache.as_ref().filter(|path| path.is_file()) {
            // Cached provider data has the same byte budget as a live response.
            let file = std::fs::File::open(path)?;
            if !file.metadata()?.is_file() {
                bail!("OpenRouter cache must be a regular file");
            }
            let cached: Value = serde_json::from_slice(&bounded_response(file)?)
                .context("invalid OpenRouter cache JSON")?;
            return Ok(ApiResponse {
                body: cached,
                cached: true,
            });
        }
        let token = self.key.as_deref().context("set OPENROUTER_API_KEY to enable this paid OpenRouter request (local inspection and cached results need no key)")?;
        eprintln!(
            "OpenRouter: requesting {} from {}",
            endpoint,
            body["model"].as_str().unwrap_or("selected model")
        );
        let response = self
            .client
            .post(format!("{}/{endpoint}", self.base_url))
            .bearer_auth(token)
            .header("Content-Type", "application/json")
            .header("X-Title", "Skywrite")
            .body(sent)
            .send()
            .context("OpenRouter request failed")?;
        let status = response.status();
        let bytes = bounded_response(response)?;
        if !status.is_success() {
            let message: String = String::from_utf8_lossy(&bytes).chars().take(2048).collect();
            if status.as_u16() == 404
                && (message.contains("data policy") || message.contains("No endpoints"))
            {
                bail!(
                    "OpenRouter HTTP {}: {}\nNo provider of this model satisfies the data policy ({}). Choose another model, or set {DATA_POLICY_ENV}=no-training or provider-default to relax it.",
                    status.as_u16(),
                    message,
                    policy_name()
                );
            }
            bail!("OpenRouter HTTP {}: {}", status.as_u16(), message);
        }
        let parsed: Value =
            serde_json::from_slice(&bytes).context("OpenRouter returned invalid JSON")?;
        if parsed.get("error").is_some() {
            bail!("OpenRouter returned an error: {}", parsed["error"]);
        }
        if let Some(path) = cache {
            match crate::io::atomic_write(&path, &serde_json::to_vec(&parsed)?, false) {
                Ok(()) => {}
                Err(_) if path.is_file() => {}
                Err(e) => return Err(e).context("saving OpenRouter response cache"),
            }
        }
        Ok(ApiResponse {
            body: parsed,
            cached: false,
        })
    }
}

pub const DATA_POLICY_ENV: &str = "SKYWRITE_DATA_POLICY";

fn policy_name() -> String {
    std::env::var(DATA_POLICY_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "zero-retention".into())
}

/// Provider routing for requests that carry the user's media or words.
/// Default: only providers that neither train on nor retain the data.
pub fn data_policy() -> Result<Option<Value>> {
    Ok(match policy_name().as_str() {
        "zero-retention" => Some(serde_json::json!({"data_collection": "deny", "zdr": true})),
        "no-training" => Some(serde_json::json!({"data_collection": "deny"})),
        "provider-default" => None,
        other => bail!(
            "{DATA_POLICY_ENV} must be zero-retention, no-training or provider-default, not {other:?}"
        ),
    })
}

pub fn cache_key(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn assistant_json(response: &Value) -> Result<Value> {
    let content = response["choices"][0]["message"]["content"]
        .as_str()
        .context("model returned no text content")?;
    let content = content.trim();
    let content = content
        .strip_prefix("```json")
        .or_else(|| content.strip_prefix("```"))
        .and_then(|s| s.trim_end().strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(content);
    serde_json::from_str(content)
        .context("model did not return valid JSON; select a model supporting JSON output")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_separates_endpoint_model_and_payload() {
        assert_ne!(cache_key(&[b"a", b"bc"]), cache_key(&[b"ab", b"c"]));
        assert_ne!(
            cache_key(&[b"model-a", b"audio"]),
            cache_key(&[b"model-b", b"audio"])
        );
        assert_ne!(
            cache_key(&[b"frames", b"one"]),
            cache_key(&[b"frames", b"two"])
        );
    }
    #[test]
    fn extracts_json_without_inventing_model_output() {
        assert_eq!(assistant_json(&serde_json::json!({"choices":[{"message":{"content":"```json\n{\"summary\":\"hi\"}\n```"}}]})).unwrap()["summary"], "hi");
        assert!(assistant_json(&serde_json::json!({"choices":[]})).is_err());
    }
    #[test]
    fn request_cache_uses_actual_content_and_needs_no_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut api = OpenRouter::new(Some(dir.path())).unwrap();
        api.key = None;
        let request = serde_json::json!({"model":"test", "messages":[]});
        let encoded = serde_json::to_vec(&request).unwrap();
        let hash = cache_key(&[API_BASE.as_bytes(), b"chat/completions", &encoded]);
        crate::io::write_json(
            &dir.path().join(format!("{hash}.json")),
            &serde_json::json!({"cached":"yes"}),
            false,
        )
        .unwrap();
        let response = api.request("chat/completions", &request).unwrap();
        assert!(response.cached);
        assert_eq!(response.body["cached"], "yes");
        assert!(
            api.request(
                "chat/completions",
                &serde_json::json!({"model":"different"})
            )
            .unwrap_err()
            .to_string()
            .contains("OPENROUTER_API_KEY")
        );
    }
    // A single owned loopback request, with real reqwest transport and bounded
    // server lifetime. No runtime credentials or remote endpoints are involved.
    fn fixture(
        reply: Vec<u8>,
        before_headers: Duration,
        before_body: Duration,
    ) -> (OpenRouter, std::thread::JoinHandle<()>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            let header = String::from_utf8(request).unwrap();
            assert!(
                header
                    .to_ascii_lowercase()
                    .contains("authorization: bearer dummy_test_only")
            );
            let length: usize = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|s| s.trim().parse().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap()["model"],
                "fixture"
            );
            std::thread::sleep(before_headers);
            let split = reply.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let _ = stream.write_all(&reply[..split]);
            std::thread::sleep(before_body);
            let _ = stream.write_all(&reply[split..]);
        });
        (
            OpenRouter::for_test(base, None, Duration::from_millis(120)),
            handle,
        )
    }
    fn reply(status: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }
    #[test]
    fn actual_http_success_cache_and_non_success() {
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(15));
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(180));
        let dir = tempfile::tempdir().unwrap();
        let (mut api, server) = fixture(
            reply("200 OK", br#"{"ok":true}"#),
            Duration::ZERO,
            Duration::ZERO,
        );
        api.cache_dir = Some(dir.path().to_owned());
        let request = serde_json::json!({"model":"fixture"});
        assert!(!api.request("chat/completions", &request).unwrap().cached);
        server.join().unwrap();
        // The one-shot listener is gone: this second result must come from cache.
        let second = api.request("chat/completions", &request).unwrap();
        assert!(second.cached);
        assert_eq!(second.body["ok"], true);
        let (api, server) = fixture(
            reply("422 Unprocessable Entity", b"fixture rejected"),
            Duration::ZERO,
            Duration::ZERO,
        );
        let error = api
            .request("chat/completions", &request)
            .unwrap_err()
            .to_string();
        assert!(error.contains("HTTP 422") && error.contains("fixture rejected"));
        server.join().unwrap();
    }
    #[test]
    fn actual_http_does_not_follow_redirects() {
        let destination = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let bytes = format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{}/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", destination.local_addr().unwrap()).into_bytes();
        let (api, server) = fixture(bytes, Duration::ZERO, Duration::ZERO);
        assert!(
            api.request("chat/completions", &serde_json::json!({"model":"fixture"}))
                .unwrap_err()
                .to_string()
                .contains("HTTP 307")
        );
        server.join().unwrap();
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[test]
    fn actual_http_exact_body_cap_and_first_excess_byte() {
        let mut body = b"{\"data\":\"".to_vec();
        body.resize(MAX_RESPONSE_BYTES as usize - 2, b'a');
        body.extend_from_slice(b"\"}");
        assert_eq!(body.len(), MAX_RESPONSE_BYTES as usize);
        for excess in [false, true] {
            let mut bytes = body.clone();
            if excess {
                bytes.push(b' ');
            }
            let (mut api, server) =
                fixture(reply("200 OK", &bytes), Duration::ZERO, Duration::ZERO);
            // Large local transfers need an ordinary deadline, not the deliberately
            // short deadline used to discriminate the stall tests below.
            api.client = http_client(CONNECT_TIMEOUT, Duration::from_secs(5)).unwrap();
            let result = api.request("chat/completions", &serde_json::json!({"model":"fixture"}));
            if excess {
                assert!(result.unwrap_err().to_string().contains("exceeded 8 MiB"));
            } else {
                assert_eq!(
                    result.unwrap().body["data"].as_str().unwrap().len(),
                    MAX_RESPONSE_BYTES as usize - 11
                );
            }
            server.join().unwrap();
        }
    }
    #[test]
    fn actual_http_deadline_covers_slow_headers_and_body() {
        for (headers, body) in [
            (Duration::from_millis(350), Duration::ZERO),
            (Duration::ZERO, Duration::from_millis(350)),
        ] {
            let (api, server) = fixture(reply("200 OK", br#"{"ok":true}"#), headers, body);
            let started = std::time::Instant::now();
            let error = api
                .request("chat/completions", &serde_json::json!({"model":"fixture"}))
                .unwrap_err();
            assert!(started.elapsed() >= Duration::from_millis(80));
            assert!(started.elapsed() < Duration::from_secs(2));
            assert!(format!("{error:#}").contains("timed out"), "{error:#}");
            server.join().unwrap();
        }
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn public_catalog_get_omits_auth_and_rejects_unlisted_routes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api = OpenRouter::for_test(
            format!("http://{}", listener.local_addr().unwrap()),
            None,
            Duration::from_secs(2),
        );
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            let text = String::from_utf8(request).unwrap().to_lowercase();
            assert!(text.starts_with("get /videos/models http/1.1"));
            assert!(!text.contains("authorization:"));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}").unwrap();
        });
        assert_eq!(
            api.catalog("videos/models").unwrap()["data"],
            serde_json::json!([])
        );
        server.join().unwrap();
        assert!(api.catalog("https://example.invalid").is_err());
        assert!(api.catalog("../credits").is_err());
    }
}
