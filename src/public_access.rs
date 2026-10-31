//! Explicit anonymous, bounded diagnostics; this never certifies a provider fetch.
use anyhow::{Context, Result, ensure};
use reqwest::{blocking::Client, header};
use serde::Serialize;
use std::{io::Read, time::Duration};
use url::Url;

pub const PREFIX_BYTES: usize = 1024;
const MAX_REDIRECTS: usize = 4;

#[derive(Debug)]
pub struct CheckFailed {
    pub report: serde_json::Value,
}
impl std::fmt::Display for CheckFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "anonymous public access check failed")
    }
}
impl std::error::Error for CheckFailed {}

pub(crate) fn checked_result(
    report: serde_json::Value,
    success: bool,
) -> Result<serde_json::Value> {
    if success {
        Ok(report)
    } else {
        Err(CheckFailed { report }.into())
    }
}

#[derive(Debug, Serialize)]
pub struct PublicAccessReport {
    pub http_status: u16,
    pub content_type: Option<String>,
    pub content_range: Option<String>,
    pub requested_range: &'static str,
    pub bytes_checked: usize,
    pub redirects: usize,
    pub body_limit_exceeded: bool,
    pub anonymous_read_succeeded: bool,
    pub prefix_matches: Option<bool>,
    pub full_sha256_verified: bool,
    pub provider_fetch_verified: bool,
}

/// No credentials, cookies, proxy, or provider requests. Every redirect gets a
/// fresh client from the existing HTTPS/global-address/IP-pinning boundary.
pub fn check(
    raw: &str,
    expected_prefix: Option<&[u8]>,
    expected_bytes: Option<u64>,
) -> Result<PublicAccessReport> {
    check_with(raw, expected_prefix, expected_bytes, |url| {
        crate::generation::public_download_client(url)
    })
}

fn range_length(raw: Option<&str>, expected_bytes: Option<u64>) -> Option<usize> {
    let (range, total) = raw?.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.parse().ok()?;
    let end: u64 = end.parse().ok()?;
    let total: u64 = total.parse().ok()?;
    let length = end.checked_add(1)?;
    if start != 0
        || end >= PREFIX_BYTES as u64
        || end >= total
        || length != total.min(PREFIX_BYTES as u64)
        || expected_bytes.is_some_and(|expected| expected != total)
    {
        return None;
    }
    usize::try_from(length).ok()
}

fn check_with(
    raw: &str,
    expected_prefix: Option<&[u8]>,
    expected_bytes: Option<u64>,
    mut client_for: impl FnMut(&Url) -> Result<Client>,
) -> Result<PublicAccessReport> {
    ensure!(raw.len() <= 4096, "public URL exceeds 4096 bytes");
    ensure!(
        expected_prefix.is_none_or(|p| p.len() <= PREFIX_BYTES),
        "expected public prefix exceeds 1024 bytes"
    );
    let mut url = Url::parse(raw).context("invalid public URL")?;
    for redirects in 0..=MAX_REDIRECTS {
        let mut response = client_for(&url)?
            .get(url.clone())
            .timeout(Duration::from_secs(30))
            .header(
                header::USER_AGENT,
                concat!("skywrite/", env!("CARGO_PKG_VERSION")),
            )
            .header(header::RANGE, "bytes=0-1023")
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .context("anonymous public range request failed")?;
        if response.status().is_redirection() {
            ensure!(redirects < MAX_REDIRECTS, "too many public URL redirects");
            let location = response
                .headers()
                .get(header::LOCATION)
                .context("public redirect lacks Location")?
                .to_str()?;
            ensure!(
                location.len() <= 4096,
                "public redirect URL exceeds 4096 bytes"
            );
            url = url.join(location)?;
            ensure!(
                url.as_str().len() <= 4096,
                "public redirect URL exceeds 4096 bytes"
            );
            continue;
        }
        let status = response.status();
        let header_text = |name: header::HeaderName| -> Option<String> {
            response
                .headers()
                .get(name)?
                .to_str()
                .ok()
                .map(|s| s.chars().take(512).collect())
        };
        let content_type = header_text(header::CONTENT_TYPE);
        let content_range = header_text(header::CONTENT_RANGE);
        let content_encoding = header_text(header::CONTENT_ENCODING);
        let mut bytes = Vec::with_capacity(PREFIX_BYTES + 1);
        response
            .by_ref()
            .take((PREFIX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        let body_limit_exceeded = bytes.len() > PREFIX_BYTES;
        let range_valid = if status.as_u16() == 206 {
            range_length(content_range.as_deref(), expected_bytes) == Some(bytes.len())
        } else {
            status.as_u16() == 200 && expected_bytes.is_none_or(|n| n == bytes.len() as u64)
        };
        let prefix_matches = expected_prefix.map(|prefix| bytes == prefix);
        return Ok(PublicAccessReport {
            http_status: status.as_u16(),
            content_type,
            content_range,
            requested_range: "bytes=0-1023",
            bytes_checked: bytes.len().min(PREFIX_BYTES),
            redirects,
            body_limit_exceeded,
            anonymous_read_succeeded: range_valid
                && !body_limit_exceeded
                && content_encoding
                    .as_deref()
                    .is_none_or(|encoding| encoding == "identity")
                && prefix_matches != Some(false),
            prefix_matches,
            full_sha256_verified: false,
            provider_fetch_verified: false,
        });
    }
    unreachable!("redirect count is bounded")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::BufRead,
        path::Path,
        process::{Child, Command, Stdio},
    };

    struct OwnedTls {
        child: Child,
        address: std::net::SocketAddr,
    }
    impl Drop for OwnedTls {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    impl OwnedTls {
        fn start() -> Self {
            let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/network");
            let mut child = Command::new("python3")
                .arg(base.join("public_server.py"))
                .arg(base.join("test-server.pem"))
                .arg(base.join("test-server.key"))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            Self {
                child,
                address: format!("127.0.0.1:{}", line.trim()).parse().unwrap(),
            }
        }
        fn client(&self, url: &Url) -> Result<Client> {
            crate::generation::validate_public_url(url)?;
            if url.host_str() != Some("pinned.invalid") {
                return crate::generation::public_download_client(url);
            }
            let cert = reqwest::Certificate::from_pem(include_bytes!(
                "../tests/fixtures/network/test-ca.pem"
            ))?;
            Ok(
                crate::generation::public_test_client("pinned.invalid", &[self.address])
                    .tls_certs_merge([cert])
                    .timeout(Duration::from_secs(2))
                    .build()?,
            )
        }
        fn check(
            &self,
            path: &str,
            expected: Option<&[u8]>,
            bytes: Option<u64>,
        ) -> Result<PublicAccessReport> {
            check_with(
                &format!("https://pinned.invalid{path}"),
                expected,
                bytes,
                |url| self.client(url),
            )
        }
    }

    #[test]
    fn content_ranges_reject_extreme_and_invalid_values_without_panicking() {
        assert_eq!(range_length(None, None), None);
        for raw in [
            "",
            "bytes */8192",
            "bytes 0-0/*",
            "bytes 0-0/0",
            "bytes 1-1023/8192",
            "bytes 0-1024/8192",
            "bytes 0-1022/8192",
            "bytes 0-2/2",
            "bytes 0-18446744073709551615/18446744073709551615",
            "bytes 18446744073709551615-18446744073709551615/18446744073709551615",
            "bytes 0-18446744073709551616/18446744073709551615",
            "bytes 0-1023/18446744073709551616",
        ] {
            assert_eq!(range_length(Some(raw), None), None, "{raw}");
        }
        assert_eq!(range_length(Some("bytes 0-1023/8192"), Some(9000)), None);
    }

    #[test]
    fn content_ranges_accept_full_prefix_and_complete_short_objects() {
        for (raw, expected_total, length) in [
            ("bytes 0-1023/8192", 8192, PREFIX_BYTES),
            ("bytes 0-1023/1024", 1024, PREFIX_BYTES),
            ("bytes 0-1023/18446744073709551615", u64::MAX, PREFIX_BYTES),
            ("bytes 0-1/2", 2, 2),
            ("bytes 0-0/1", 1, 1),
        ] {
            assert_eq!(range_length(Some(raw), None), Some(length), "{raw}");
            assert_eq!(
                range_length(Some(raw), Some(expected_total)),
                Some(length),
                "{raw}"
            );
        }
    }

    #[test]
    fn anonymous_owned_tls_success_range_prefix_and_no_credentials() {
        let server = OwnedTls::start();
        let prefix = vec![b'x'; PREFIX_BYTES];
        let report = server.check("/success", Some(&prefix), Some(8192)).unwrap();
        assert_eq!(report.http_status, 206);
        assert!(report.anonymous_read_succeeded);
        assert_eq!(report.prefix_matches, Some(true));
        assert_eq!(report.content_type.as_deref(), Some("video/mp4"));
        assert_eq!(report.bytes_checked, 1024);
        assert!(!report.full_sha256_verified && !report.provider_fetch_verified);
        // Legitimate small objects can ignore Range and return a complete 200.
        let small = server.check("/small", Some(b"ok"), Some(2)).unwrap();
        assert!(small.anonymous_read_succeeded);
        assert_eq!(small.http_status, 200);
        let mismatch = server
            .check("/success", Some(&vec![b'y'; PREFIX_BYTES]), Some(8192))
            .unwrap();
        assert_eq!(mismatch.prefix_matches, Some(false));
        assert!(!mismatch.anonymous_read_succeeded);
        assert!(
            !server
                .check("/success", Some(&prefix), Some(9000))
                .unwrap()
                .anonymous_read_succeeded
        );
    }

    #[test]
    fn anonymous_owned_tls_denied_oversize_truncated_and_invalid_ranges() {
        let server = OwnedTls::start();
        let denied = server.check("/denied", None, None).unwrap();
        assert_eq!(denied.http_status, 403);
        assert!(!denied.anonymous_read_succeeded);
        for path in ["/oversized", "/ignored-range"] {
            let report = server.check(path, None, None).unwrap();
            assert!(report.body_limit_exceeded, "{path}");
            assert_eq!(report.bytes_checked, PREFIX_BYTES);
            assert!(!report.anonymous_read_succeeded);
        }
        for path in ["/short", "/wrong-range"] {
            assert!(
                !server
                    .check(path, None, None)
                    .unwrap()
                    .anonymous_read_succeeded,
                "{path}"
            );
        }
        let failure =
            checked_result(serde_json::json!({"public_access":denied}), false).unwrap_err();
        assert_eq!(
            failure.downcast_ref::<CheckFailed>().unwrap().report["public_access"]["http_status"],
            403
        );
    }

    #[test]
    fn anonymous_owned_tls_redirects_revalidate_every_hop_without_cookie_forwarding() {
        let server = OwnedTls::start();
        let redirected = server.check("/redirect", None, None).unwrap();
        assert!(redirected.anonymous_read_succeeded);
        assert_eq!(redirected.redirects, 1);
        for (path, reason) in [
            ("/private", "nonpublic"),
            ("/http", "public HTTPS"),
            ("/userinfo", "public HTTPS"),
            ("/loop", "too many"),
        ] {
            let error = server.check(path, None, None).unwrap_err();
            assert!(error.to_string().contains(reason), "{path}: {error:#}");
        }
        for raw in [
            "http://example.com/",
            "https://localhost/",
            "https://127.0.0.1/",
            "https://user:secret@example.com/",
        ] {
            assert!(check(raw, None, None).is_err(), "{raw}");
        }
    }
}
