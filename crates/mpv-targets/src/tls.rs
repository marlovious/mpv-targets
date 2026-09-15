use rustls::ServerConfig;
use std::io::{self, Cursor};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TlsMaterialError {
    #[error("cannot parse TLS material: {0}")]
    Parse(#[source] io::Error),
    #[error("certificate file contains no certificates")]
    NoCertificate,
    #[error("private-key file contains no private key")]
    NoPrivateKey,
    #[error("certificate and private key cannot be used together: {0}")]
    InvalidPair(#[source] rustls::Error),
}

pub fn server_tls_config(
    certificate: &[u8],
    private_key: &[u8],
) -> Result<ServerConfig, TlsMaterialError> {
    let certificates = rustls_pemfile::certs(&mut Cursor::new(certificate))
        .collect::<Result<Vec<_>, _>>()
        .map_err(TlsMaterialError::Parse)?;
    if certificates.is_empty() {
        return Err(TlsMaterialError::NoCertificate);
    }
    let private_key = rustls_pemfile::private_key(&mut Cursor::new(private_key))
        .map_err(TlsMaterialError::Parse)?
        .ok_or(TlsMaterialError::NoPrivateKey)?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, private_key)
        .map_err(TlsMaterialError::InvalidPair)
}
