//! WebRTC DTLS certificate.
//!
//! The DTLS certificate determines the node's WebRTC `certhash` and therefore its
//! advertised multiaddrs. Generating a fresh certificate on every start gives the node
//! a new identity each time, reusing one keeps that identity stable across restarts.
//!
//! litep2p does not persist the certificate itself, persistence is left to the caller.

use str0m::{config::DtlsCert, crypto::CryptoError, error::DtlsError, RtcError};

#[derive(Debug)]
pub struct DtlsCertificate {
    certificate: Vec<u8>,
    private_key: Vec<u8>,
}

impl DtlsCertificate {
    /// Reconstructs a certificate from previously stored bytes.
    pub fn load(certificate: Vec<u8>, private_key: Vec<u8>) -> crate::Result<Self> {
        // NOTE: here certificate and private_key could be validated.
        Ok(Self {
            certificate,
            private_key,
        })
    }

    /// Generates a fresh WebRTC DTLS certificate.
    pub fn new() -> crate::Result<Self> {
        let dtls_cert =
            str0m::crypto::from_feature_flags().dtls_provider.generate_certificate().ok_or(
                crate::error::Error::WebRtc(RtcError::Dtls(DtlsError::CryptoError(
                    CryptoError::Other("OpenSsl failed to generate certificate".to_string()),
                ))),
            )?;
        Ok(Self {
            certificate: dtls_cert.certificate,
            private_key: dtls_cert.private_key,
        })
    }

    /// Returns the raw certificate and private-key bytes.
    pub fn as_parts(&self) -> (&Vec<u8>, &Vec<u8>) {
        (&self.certificate, &self.private_key)
    }
}

impl From<DtlsCertificate> for DtlsCert {
    fn from(cert: DtlsCertificate) -> Self {
        Self {
            certificate: cert.certificate,
            private_key: cert.private_key,
        }
    }
}
