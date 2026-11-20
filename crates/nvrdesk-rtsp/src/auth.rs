//! HTTP Digest (and Basic fallback) authentication for RTSP requests.

use std::collections::HashMap;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use md5::{Digest, Md5};

const REALM: &str = "NVRDesk";

/// Credentials for RTSP Digest authentication.
#[derive(Debug, Clone)]
pub struct RtspAuthConfig {
    pub username: String,
    pub password: String,
}

impl RtspAuthConfig {
    /// Builds a `WWW-Authenticate` Digest challenge header value for `nonce`.
    #[must_use]
    pub fn challenge_header(nonce: &str) -> String {
        format!(r#"Digest realm="{REALM}", nonce="{nonce}", algorithm=MD5, qop="auth""#)
    }

    /// Verifies an `Authorization` header against `method` (e.g. `"DESCRIBE"`) and `expected_nonce`.
    #[must_use]
    pub fn verify_authorization(
        &self,
        auth_header: &str,
        method: &str,
        expected_nonce: &str,
    ) -> bool {
        let trimmed = auth_header.trim();
        if let Some(rest) = strip_prefix_ignore_ascii_case(trimmed, "Digest ") {
            return self.verify_digest(rest, method, expected_nonce);
        }
        if let Some(rest) = strip_prefix_ignore_ascii_case(trimmed, "Basic ")
            && let Ok(decoded) = BASE64.decode(rest.trim())
            && let Ok(creds) = String::from_utf8(decoded)
            && let Some((user, pass)) = creds.split_once(':')
        {
            return user == self.username && pass == self.password;
        }
        false
    }

    fn verify_digest(&self, params_str: &str, method: &str, expected_nonce: &str) -> bool {
        let params = parse_digest_params(params_str);
        let Some(username) = params.get("username") else {
            return false;
        };
        let Some(nonce) = params.get("nonce") else {
            return false;
        };
        let Some(uri) = params.get("uri") else {
            return false;
        };
        let Some(response) = params.get("response") else {
            return false;
        };

        if username != &self.username || nonce != expected_nonce {
            return false;
        }
        if let Some(realm) = params.get("realm")
            && realm != REALM
        {
            return false;
        }

        let ha1 = md5_hex(&format!("{}:{}:{}", self.username, REALM, self.password));
        let ha2 = md5_hex(&format!("{method}:{uri}"));

        let expected = if let Some(qop) = params.get("qop") {
            let Some(nc) = params.get("nc") else {
                return false;
            };
            let Some(cnonce) = params.get("cnonce") else {
                return false;
            };
            md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}"))
        } else {
            md5_hex(&format!("{ha1}:{nonce}:{ha2}"))
        };

        expected.eq_ignore_ascii_case(response)
    }
}

fn md5_hex(input: &str) -> String {
    let digest = Md5::digest(input.as_bytes());
    let mut out = String::with_capacity(32);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn parse_digest_params(input: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut rest = input.trim();

    while !rest.is_empty() {
        let Some(eq_idx) = rest.find('=') else {
            break;
        };
        let key = rest[..eq_idx].trim().to_ascii_lowercase();
        rest = rest[eq_idx + 1..].trim_start();

        let value = if let Some(after_quote) = rest.strip_prefix('"') {
            if let Some(end_quote) = after_quote.find('"') {
                let val = &after_quote[..end_quote];
                rest = after_quote[end_quote + 1..].trim_start();
                if let Some(after_comma) = rest.strip_prefix(',') {
                    rest = after_comma.trim_start();
                }
                val.to_string()
            } else {
                let val = after_quote.to_string();
                rest = "";
                val
            }
        } else if let Some(comma_idx) = rest.find(',') {
            let val = rest[..comma_idx].trim().to_string();
            rest = rest[comma_idx + 1..].trim_start();
            val
        } else {
            let val = rest.trim().to_string();
            rest = "";
            val
        };

        map.insert(key, value);
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_rfc2617_digest_with_qop_and_legacy() {
        let cfg = RtspAuthConfig {
            username: "admin".into(),
            password: "secret".into(),
        };
        let nonce = "abcdef123456";
        let uri = "rtsp://127.0.0.1:8554/stream1";
        let ha1 = md5_hex("admin:NVRDesk:secret");
        let ha2 = md5_hex(&format!("DESCRIBE:{uri}"));

        // With qop=auth
        let resp_qop = md5_hex(&format!("{ha1}:{nonce}:00000001:deadbeef:auth:{ha2}"));
        let hdr_qop = format!(
            r#"Digest username="admin", realm="NVRDesk", nonce="{nonce}", uri="{uri}", qop=auth, nc=00000001, cnonce="deadbeef", response="{resp_qop}""#
        );
        assert!(cfg.verify_authorization(&hdr_qop, "DESCRIBE", nonce));

        // Legacy without qop
        let resp_legacy = md5_hex(&format!("{ha1}:{nonce}:{ha2}"));
        let hdr_legacy = format!(
            r#"Digest username="admin", realm="NVRDesk", nonce="{nonce}", uri="{uri}", response="{resp_legacy}""#
        );
        assert!(cfg.verify_authorization(&hdr_legacy, "DESCRIBE", nonce));

        // Wrong password/nonce rejected
        assert!(!cfg.verify_authorization(&hdr_qop, "DESCRIBE", "wrongnonce"));
    }
}
