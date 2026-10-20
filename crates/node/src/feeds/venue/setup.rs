//! The TLS client config the venue connections share.

use std::sync::Arc;

use anyhow::Context;
use rustls::crypto::ring;
use rustls::{ClientConfig, RootCertStore};

/// The TLS client config every venue connection shares: ring as the crypto provider, and
/// Mozilla's roots compiled into the binary, so the host's certificate store is never trusted.
///
/// TODO: certificate validity is judged by the untrusted host clock and revocation is not
/// checked, so a host holding an expired or revoked venue key could set the clock back and pose
/// as the venue; a trusted time bound closes the first.
/// Error cases:
/// - the provider supports none of the safe default protocol versions
pub fn tls_config() -> anyhow::Result<Arc<ClientConfig>> {
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}
