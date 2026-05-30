//! Persistence of the WebRTC DTLS certificate.
//!
//! The DTLS certificate determines the node's WebRTC `certhash` and therefore its
//! advertised `/webrtc-direct` multiaddrs. Persisting it across restarts keeps that
//! identity stable, without persistence a fresh certificate is generated
//!  on every start.
//!
//! [`DtlsCertCache`] is the entry point, [`DtlsCertCache::new`] inspects the
//! configured folder, and [`DtlsCertCache::build`] returns the certificate to use,
//! loading the persisted one when available and otherwise generating a new one.

use crate::transport::webrtc::LOG_TARGET;
use std::{
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
};
use str0m::{config::DtlsCert, crypto::CryptoError, error::DtlsError, RtcError};

/// Name of the file, inside the configured persistence folder, that stores the
/// DTLS certificate and private key.
const DTLS_CERT_FILE: &str = "webrtc_dtlscert";

/// Result of inspecting the configured persistence folder.
enum DtlsCertState {
    /// No persistence folder was configured, always generate an ephemeral certificate.
    NonPersistent,
    /// A folder was configured but no certificate file exists yet.
    NotFound(PathBuf),
    /// A certificate file exists but could not be read or decoded.
    Invalid(PathBuf),
    /// A valid certificate was loaded from disk and will be reused as-is.
    Valid(DtlsCert),
}

/// Loads and persists the node's WebRTC DTLS certificate.
///
/// Construct with [`DtlsCertCache::new`], then call [`DtlsCertCache::build`]
/// to obtain the [`DtlsCert`] the transport should use.
pub struct DtlsCertCache {
    /// Classification of the configured folder, decided at construction time.
    cert_hash_state: DtlsCertState,
}

impl DtlsCertCache {
    /// Inspects `dtls_cert_persistent_path` and records what `build` should do.
    pub fn new(dtls_cert_persistent_path: Option<PathBuf>) -> Self {
        let cert_hash_state = match dtls_cert_persistent_path {
            Some(dtls_cert_path) => match Self::decode(&dtls_cert_path) {
                Ok(dtls_cert) => DtlsCertState::Valid(dtls_cert),
                Err(false) => DtlsCertState::Invalid(dtls_cert_path),
                Err(true) => DtlsCertState::NotFound(dtls_cert_path),
            },
            None => DtlsCertState::NonPersistent,
        };

        DtlsCertCache { cert_hash_state }
    }

    /// Returns the [`DtlsCert`] to use, according to the state.
    ///
    /// Returns an error only when certificate generation fails.
    pub fn build(self, crypto: &str0m::crypto::CryptoProvider) -> crate::Result<DtlsCert> {
        match self.cert_hash_state {
            DtlsCertState::Valid(dtls_cert) => Ok(dtls_cert),
            DtlsCertState::NotFound(dtls_cert_path) => {
                tracing::info!(
                        target: LOG_TARGET,
                        dtls_cert_path = ?dtls_cert_path.clone(),
                        "dtls cert didn't exist, generating new one");
                let dtls_cert = Self::generate(crypto)?;
                if let Err(e) = Self::encode(&dtls_cert_path, &dtls_cert) {
                    tracing::debug!(
                            target: LOG_TARGET,
                            ?dtls_cert_path ,
                            ?e,
                            "failed to encode dtls cert to the specified path");
                }
                Ok(dtls_cert)
            }
            DtlsCertState::Invalid(dtls_cert_path) => {
                tracing::warn!(
                        target: LOG_TARGET,
                        ?dtls_cert_path ,
                        "failed to decode dtls cert, not overwriting it");
                Self::generate(crypto)
            }
            DtlsCertState::NonPersistent => Self::generate(crypto),
        }
    }

    /// Generates a fresh self-signed DTLS certificate using `crypto`.
    fn generate(crypto: &str0m::crypto::CryptoProvider) -> crate::Result<DtlsCert> {
        crypto.dtls_provider.generate_certificate().ok_or(crate::error::Error::WebRtc(
            RtcError::Dtls(DtlsError::CryptoError(CryptoError::Other(
                "OpenSsl failed to generate certificate".to_string(),
            ))),
        ))
    }

    /// Encodes `dtls_cert` into the `webrtc_certhash` file inside `path`.
    ///
    /// Layout, lengths as big-endian `u64`:
    /// `cert len ++ cert bytes ++ key len ++ key bytes`.
    ///
    /// Errors if `path` does not exist.
    fn encode(path: &Path, dtls_cert: &DtlsCert) -> std::io::Result<()> {
        if !path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "dtls cert folder doesn't exist",
            ));
        }

        let path = path.join(DTLS_CERT_FILE);

        let mut file =
            std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(path)?;

        file.write_all(&(dtls_cert.certificate.len() as u64).to_be_bytes()[..])?;
        file.write_all(&dtls_cert.certificate[..])?;
        file.write_all(&(dtls_cert.private_key.len() as u64).to_be_bytes()[..])?;
        file.write_all(&dtls_cert.private_key[..])?;

        Ok(())
    }

    /// Decodes a [`DtlsCert`] from the `webrtc_certhash` file inside `path`.
    ///
    /// Layout, lengths as big-endian `u64`:
    /// `cert len ++ cert bytes ++ key len ++ key bytes`.
    ///
    /// The `bool` in the error says whether the caller may persist a fresh cert:
    /// - true: file absent, generate and save a new one.
    /// - false: file present but unreadable/corrupt, do NOT overwrite it
    fn decode(path: &Path) -> Result<DtlsCert, bool> {
        let path = path.join(DTLS_CERT_FILE);

        let mut file = std::fs::OpenOptions::new().read(true).open(path.clone()).map_err(|e| {
            tracing::debug!(?path, ?e, "failed opening dtls cert file");
            // Create the new dtls cert file only if the file was not existing before.
            match e.kind() {
                ErrorKind::NotFound => true,
                _ => false,
            }
        })?;

        let file_len = file
            .metadata()
            .map_err(|e| {
                tracing::debug!(?path, ?e, "failed access to dtls cert file metadata");
                false
            })
            .map(|metadata| metadata.len() as usize)?;

        let mut read = |buf: &mut [u8]| {
            file.read_exact(buf).map_err(|e| {
                tracing::debug!(?path, ?e, "failed reading buffer from file");
                false
            })
        };

        let mut buf = [0; 8];
        read(&mut buf)?;
        let certificate_len = u64::from_be_bytes(buf) as usize;
        if certificate_len > file_len {
            tracing::debug!(?path, "decoded certificate_len is too big");
            return Err(false);
        }
        let mut certificate = vec![0; certificate_len];
        read(&mut certificate)?;

        read(&mut buf)?;
        let private_key_len = u64::from_be_bytes(buf) as usize;
        // 16 = 2 length u64 prefixes
        let expected_private_key_len = match (file_len - certificate_len).checked_sub(16) {
            Some(expected) => expected,
            None => {
                tracing::debug!(?path, "certificate key len was too long");
                return Err(false);
            }
        };
        if private_key_len != expected_private_key_len {
            tracing::debug!(?path, "decoded private key len doesn't match expected one");
            return Err(false);
        }
        let mut private_key = vec![0; private_key_len];
        read(&mut private_key)?;

        Ok(DtlsCert {
            certificate,
            private_key,
        })
    }
}
