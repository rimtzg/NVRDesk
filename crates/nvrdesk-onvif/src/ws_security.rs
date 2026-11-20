//! WS-Security UsernameToken (PasswordDigest & PasswordText) verification for ONVIF SOAP.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use quick_xml::{Reader, events::Event};
use sha1::{Digest, Sha1};

/// Credentials required for ONVIF SOAP and HTTP snapshot access.
#[derive(Debug, Clone)]
pub struct OnvifAuthConfig {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Default)]
struct ParsedToken {
    username: Option<String>,
    password: Option<String>,
    password_type: Option<String>,
    nonce: Option<String>,
    created: Option<String>,
}

impl OnvifAuthConfig {
    /// Verifies a SOAP envelope's `<wsse:Security><wsse:UsernameToken>` credentials.
    #[must_use]
    pub fn verify_soap_xml(&self, xml: &str) -> bool {
        let Some(token) = parse_username_token(xml) else {
            return false;
        };
        let Some(username) = token.username.as_deref() else {
            return false;
        };
        let Some(password) = token.password.as_deref() else {
            return false;
        };
        if username != self.username {
            return false;
        }

        let is_text = token
            .password_type
            .as_deref()
            .is_some_and(|t| t.ends_with("PasswordText"));

        if is_text || (token.nonce.is_none() && token.created.is_none()) {
            return password == self.password;
        }

        let (Some(nonce_b64), Some(created)) = (token.nonce.as_deref(), token.created.as_deref())
        else {
            return false;
        };
        let Ok(nonce_bytes) = BASE64.decode(nonce_b64.trim()) else {
            return false;
        };

        let expected = compute_password_digest(&nonce_bytes, created.trim(), &self.password);
        expected == password.trim()
    }

    /// Verifies an HTTP `Authorization: Basic ...` header value.
    #[must_use]
    pub fn verify_http_basic(&self, auth_header: &str) -> bool {
        let trimmed = auth_header.trim();
        let Some(rest) = trimmed
            .strip_prefix("Basic ")
            .or_else(|| trimmed.strip_prefix("basic "))
        else {
            return false;
        };
        let Ok(decoded) = BASE64.decode(rest.trim()) else {
            return false;
        };
        let Ok(creds) = String::from_utf8(decoded) else {
            return false;
        };
        let Some((user, pass)) = creds.split_once(':') else {
            return false;
        };
        user == self.username && pass == self.password
    }
}

/// Computes ONVIF WS-Security PasswordDigest: `Base64(SHA1(nonce + created + password))`.
#[must_use]
pub fn compute_password_digest(nonce: &[u8], created: &str, password: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(nonce);
    hasher.update(created.as_bytes());
    hasher.update(password.as_bytes());
    BASE64.encode(hasher.finalize())
}

fn parse_username_token(xml: &str) -> Option<ParsedToken> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut in_token = false;
    let mut current_tag: Option<String> = None;
    let mut token = ParsedToken::default();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = e.local_name().as_ref().to_string();
                if local == "UsernameToken" {
                    in_token = true;
                } else if in_token {
                    if local == "Password" {
                        for attr in e.attributes().flatten() {
                            if attr.key.local_name().as_ref() == "Type" {
                                token.password_type = attr
                                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                    .ok()
                                    .map(|v| v.into_owned());
                            }
                        }
                    }
                    current_tag = Some(local);
                }
            }
            Ok(Event::End(e)) => {
                let local = e.local_name().as_ref().to_string();
                if local == "UsernameToken" {
                    break;
                }
                if current_tag.as_deref() == Some(local.as_str()) {
                    current_tag = None;
                }
            }
            Ok(Event::Text(e)) if in_token => {
                let txt = e.xml_content(quick_xml::XmlVersion::Implicit1_0);
                match current_tag.as_deref() {
                    Some("Username") => token.username = Some(txt.into_owned()),
                    Some("Password") => token.password = Some(txt.into_owned()),
                    Some("Nonce") => token.nonce = Some(txt.into_owned()),
                    Some("Created") => token.created = Some(txt.into_owned()),
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    if token.username.is_some() {
        Some(token)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_ws_security_password_digest() {
        let auth = OnvifAuthConfig {
            username: "admin".into(),
            password: "pos_password".into(),
        };
        let nonce = b"random-nonce-123";
        let nonce_b64 = BASE64.encode(nonce);
        let created = "2026-11-20T23:50:00Z";
        let digest = compute_password_digest(nonce, created, "pos_password");

        let soap = format!(
            r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
                <s:Header>
                    <wsse:Security xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd">
                        <wsse:UsernameToken>
                            <wsse:Username>admin</wsse:Username>
                            <wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password>
                            <wsse:Nonce>{nonce_b64}</wsse:Nonce>
                            <wsu:Created xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">{created}</wsu:Created>
                        </wsse:UsernameToken>
                    </wsse:Security>
                </s:Header>
                <s:Body><tds:GetCapabilities/></s:Body>
            </s:Envelope>"#
        );

        assert!(auth.verify_soap_xml(&soap));
        assert!(!auth.verify_soap_xml(&soap.replace(&digest, "invalid_digest")));
    }
}
