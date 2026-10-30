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
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("initializing HTTPS client")?;
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
    pub fn request(&self, endpoint: &str, body: &Value) -> Result<ApiResponse> {
        if !matches!(endpoint, "audio/transcriptions" | "chat/completions") {
            bail!("unsupported OpenRouter endpoint");
        }
        let encoded = serde_json::to_vec(body)?;
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
            let cached: Value = crate::io::read_json(path)?;
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
            .body(encoded)
            .send()
            .context("OpenRouter request failed")?;
        let status = response.status();
        let mut bytes = Vec::new();
        response
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("reading OpenRouter response")?;
        if bytes.len() as u64 > MAX_RESPONSE_BYTES {
            bail!("OpenRouter response exceeded 8 MiB");
        }
        if !status.is_success() {
            let message: String = String::from_utf8_lossy(&bytes).chars().take(2048).collect();
            bail!("OpenRouter HTTP {}: {}", status.as_u16(), message);
        }
        let parsed: Value =
            serde_json::from_slice(&bytes).context("OpenRouter returned invalid JSON")?;
        if parsed.get("error").is_some() {
            bail!("OpenRouter returned an error: {}", parsed["error"]);
        }
        if let Some(path) = cache {
            match crate::io::write_json(&path, &parsed, false) {
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
}
