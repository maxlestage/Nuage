//! TLS of Nuage DB, protected against quantum computers.
//!
//! Every connection Nuage DB encrypts itself (the PostgreSQL and key-value
//! ports with `NUAGE_TLS_CERT`, the backups to S3, `nuage check` and the
//! management commands) agrees on its keys with **X25519MLKEM768** first: a
//! hybrid of X25519 and ML-KEM-768, the post-quantum key encapsulation of
//! FIPS 203. Its secret is safe as long as either half is: a conversation
//! recorded today cannot be decrypted by a quantum computer tomorrow
//! ("harvest now, decrypt later"), and if ML-KEM were ever broken, X25519
//! still holds. The implementation is aws-lc-rs (AWS-LC, whose ML-KEM is
//! FIPS 140-3 validated), through rustls; nothing here is hand-made.
//!
//! A client that does not speak it (an older PostgreSQL driver, OpenSSL
//! before 3.5) gets X25519 or P-256, as today, unless the server requires the
//! hybrid ([`PostQuantum::Required`], `NUAGE_TLS_POST_QUANTUM=required`):
//! then only TLS 1.3 with a post-quantum group is accepted.
//!
//! What the data is encrypted with on disk and in the backups (AES-256, with
//! keys of 256 bits, Argon2id, HKDF-SHA256) is symmetric: a quantum computer
//! only halves the strength of a key (Grover), which leaves 128 bits.

use std::path::Path;
use std::sync::Arc;

use memoire_orm::{Error, Result};
use rustls::NamedGroup;
use rustls::crypto::CryptoProvider;
use rustls::crypto::aws_lc_rs::{self, kx_group};

/// Whether a server accepts connections whose keys a quantum computer could
/// recover.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PostQuantum {
    /// The hybrid first, X25519 and P-256 for the clients that lack it.
    #[default]
    Preferred,
    /// The hybrid only, over TLS 1.3: a client without it is refused.
    Required,
}

impl PostQuantum {
    /// `NUAGE_TLS_POST_QUANTUM`: `preferred` (or nothing) or `required`.
    pub fn parse(text: Option<&str>) -> std::result::Result<PostQuantum, String> {
        match text.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            None | Some("" | "preferred") => Ok(PostQuantum::Preferred),
            Some("required") => Ok(PostQuantum::Required),
            Some(other) => Err(format!("NUAGE_TLS_POST_QUANTUM: `{other}` is not `preferred` or `required`")),
        }
    }
}

/// The groups that are post-quantum: hybrids, and ML-KEM alone.
pub fn is_post_quantum(group: NamedGroup) -> bool {
    matches!(
        group,
        NamedGroup::X25519MLKEM768 | NamedGroup::secp256r1MLKEM768 | NamedGroup::MLKEM768 | NamedGroup::MLKEM1024
    )
}

/// The cryptography of every TLS connection: aws-lc-rs, the hybrid
/// X25519MLKEM768 offered and chosen first.
pub fn provider() -> Arc<CryptoProvider> {
    let mut provider = aws_lc_rs::default_provider();
    provider.kx_groups = vec![kx_group::X25519MLKEM768, kx_group::X25519, kx_group::SECP256R1, kx_group::SECP384R1];
    Arc::new(provider)
}

/// [`provider`] with the post-quantum groups only.
pub fn post_quantum_provider() -> Arc<CryptoProvider> {
    let mut provider = aws_lc_rs::default_provider();
    provider.kx_groups = vec![kx_group::X25519MLKEM768, kx_group::SECP256R1MLKEM768];
    Arc::new(provider)
}

/// The TLS configuration of a server from a certificate chain and its
/// private key, as PEM files (those of Let's Encrypt, for instance).
pub fn server_config(cert_pem: &Path, key_pem: &Path, policy: PostQuantum) -> Result<Arc<rustls::ServerConfig>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let bad = |what: &str, e: &dyn std::fmt::Display| Error::Validation(format!("TLS: {what}: {e}"));
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_pem)
        .map_err(|e| bad(&cert_pem.display().to_string(), &e))?
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| bad(&cert_pem.display().to_string(), &e))?;
    let key = PrivateKeyDer::from_pem_file(key_pem).map_err(|e| bad(&key_pem.display().to_string(), &e))?;
    let builder = match policy {
        PostQuantum::Preferred => rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| bad("protocol versions", &e))?,
        // The hybrids exist in TLS 1.3 only.
        PostQuantum::Required => rustls::ServerConfig::builder_with_provider(post_quantum_provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| bad("protocol versions", &e))?,
    };
    let config = builder.with_no_client_auth().with_single_cert(certs, key).map_err(|e| bad("certificate", &e))?;
    Ok(Arc::new(config))
}

/// The TLS configuration of a client of a server on the Internet (S3, a
/// Nuage DB server): the certificates of the web's authorities, the hybrid
/// first, HTTP/1.1.
pub fn client_config() -> std::result::Result<rustls::ClientConfig, rustls::Error> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let mut config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hybrid_comes_first() {
        let groups: Vec<NamedGroup> = provider().kx_groups.iter().map(|g| g.name()).collect();
        assert_eq!(groups[0], NamedGroup::X25519MLKEM768);
        assert!(groups.contains(&NamedGroup::X25519), "older clients still connect");
        assert!(post_quantum_provider().kx_groups.iter().all(|g| is_post_quantum(g.name())));
        assert_eq!(PostQuantum::parse(None), Ok(PostQuantum::Preferred));
        assert_eq!(PostQuantum::parse(Some(" Required ")), Ok(PostQuantum::Required));
        assert!(PostQuantum::parse(Some("yes")).is_err());
    }
}
