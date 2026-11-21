//! Pure-Rust TLS server configuration builder (`rustls` + `rcgen`) for HTTPS and RTSPS.

use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

use crate::NvrDeskError;

/// Loads a PEM certificate + private key from disk, or generates a self-signed X.509 cert.
pub fn build_tls_server_config(
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
    extra_sans: &[String],
    alpn_protocols: Vec<Vec<u8>>,
) -> Result<Arc<ServerConfig>, NvrDeskError> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (certs, key) = match (cert_path, key_path) {
        (Some(cp), Some(kp)) => {
            let certs = CertificateDer::pem_file_iter(cp)
                .map_err(|e| NvrDeskError::Config(format!("failed to read TLS cert {cp:?}: {e}")))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| NvrDeskError::Config(format!("invalid TLS cert PEM: {e}")))?;
            if certs.is_empty() {
                return Err(NvrDeskError::Config(
                    "TLS cert PEM file contains no certificates".into(),
                ));
            }
            let key = PrivateKeyDer::from_pem_file(kp)
                .map_err(|e| NvrDeskError::Config(format!("failed to read TLS key {kp:?}: {e}")))?;
            (certs, key)
        }
        (None, None) => generate_self_signed(extra_sans)?,
        _ => {
            return Err(NvrDeskError::Config(
                "both tls.cert_path and tls.key_path must be set together".into(),
            ));
        }
    };

    let mut cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| NvrDeskError::Config(format!("invalid TLS credentials: {e}")))?;
    cfg.alpn_protocols = alpn_protocols;
    Ok(Arc::new(cfg))
}

fn generate_self_signed(
    extra_sans: &[String],
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), NvrDeskError> {
    let mut sans = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "nvrdesk.local".to_string(),
    ];
    for san in extra_sans {
        if !san.is_empty() && !sans.contains(san) {
            sans.push(san.clone());
        }
    }

    let certified = rcgen::generate_simple_self_signed(sans)
        .map_err(|e| NvrDeskError::Config(format!("failed to generate self-signed cert: {e}")))?;
    let cert_der = CertificateDer::from(certified.cert);
    let key_der = PrivateKeyDer::try_from(certified.key_pair.serialize_der())
        .map_err(|e| NvrDeskError::Config(format!("failed to serialize generated TLS key: {e}")))?;
    Ok((vec![cert_der], key_der))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_self_signed_tls_server_config() {
        let cfg = build_tls_server_config(None, None, &["192.168.1.50".into()], vec![])
            .expect("self-signed TLS config");
        assert!(cfg.alpn_protocols.is_empty());
    }
}
