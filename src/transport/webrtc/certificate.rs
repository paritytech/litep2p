//! Serialization of the WebRTC DTLS certificate.
//!
//! The DTLS certificate determines the node's WebRTC `certhash` and therefore its
//! advertised multiaddrs. Generating a fresh certificate on every start gives
//! the node a new identity each time, persisting one keeps that identity
//! the same across restarts.
//!
//! litep2p does not persist the certificate itself. Instead it exposes the certificate
//! as an opaque byte blob: [`generate_certificate`] produces a new certificate,
//! [`encode`] turns it into bytes the caller can store however they like, and
//! [`decode`] reconstructs it.
//!
//! Format:
//! `[u64 certificate_len]` ++ `[certificate]` ++ `[u64 private_key_len]` ++ `[private_key]`

use std::panic::catch_unwind;

use str0m::{config::DtlsCert, crypto::CryptoError, error::DtlsError, RtcError};

/// Generates a fresh self-signed DTLS certificate.
pub fn generate_certificate() -> crate::Result<DtlsCert> {
    // OpenSsl as crypto provider is specified through the 'openssl' str0m feature flag.
    str0m::crypto::from_feature_flags().dtls_provider.generate_certificate().ok_or(
        crate::error::Error::WebRtc(RtcError::Dtls(DtlsError::CryptoError(CryptoError::Other(
            "OpenSsl failed to generate certificate".to_string(),
        )))),
    )
}

/// Validates that a reconstructed certificate is usable by the DTLS stack.
pub fn validate_certificate(certificate: &DtlsCert) -> crate::Result<()> {
    catch_unwind(|| {
        let mut rtc = str0m::Rtc::builder()
            .set_dtls_cert(certificate.clone())
            .build(std::time::Instant::now());
        rtc.direct_api().start_dtls(false).unwrap();
    })
    .map(|_| ())
    .map_err(|err| crate::Error::Other(format!("{:?}", err)))
}

/// Encodes a DTLS certificate into a byte blob.
///
/// Both the certificate and the private key are stored, each prefixed
/// with its big-endian `u64` length.
pub fn encode(dtls_cert: &DtlsCert) -> Vec<u8> {
    let mut encoding = (dtls_cert.certificate.len() as u64).to_be_bytes().to_vec();
    encoding.extend(&dtls_cert.certificate);
    encoding.extend((dtls_cert.private_key.len() as u64).to_be_bytes());
    encoding.extend(&dtls_cert.private_key);
    encoding
}

/// Decodes a DTLS certificate.
///
/// Returns an error if the input is truncated or the encoded lengths are inconsistent
/// with the buffer.
pub fn decode(mut bytes: Vec<u8>) -> crate::Result<DtlsCert> {
    const LENGTH_PREFIX_BYTES: usize = 8;
    let decoding_err = |description: &str| -> crate::Result<Vec<u8>> {
        Err(crate::Error::Other(description.to_string()))
    };

    let mut decode_vec = || -> crate::Result<Vec<u8>> {
        if bytes.len() < LENGTH_PREFIX_BYTES {
            return decoding_err("invalid encoding, too few bytes for length prefix");
        }

        let mut buf = [0; LENGTH_PREFIX_BYTES];
        buf.copy_from_slice(&bytes[..LENGTH_PREFIX_BYTES]);
        let vec_len = u64::from_be_bytes(buf) as usize;
        bytes.drain(..LENGTH_PREFIX_BYTES);

        if vec_len == 0 {
            return decoding_err("invalid encoding, decoded len cannot be 0");
        } else if vec_len > bytes.len() {
            return decoding_err("invalid encoding, decoded len bigger than buffer");
        }

        let vec = bytes[..vec_len].to_vec();
        bytes.drain(..vec_len);
        Ok(vec)
    };

    Ok(DtlsCert {
        certificate: decode_vec()?,
        private_key: decode_vec()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        // generate -> encode -> decode
        let cert = generate_certificate().unwrap();
        let decoded = decode(encode(&cert)).unwrap();
        assert_eq!(cert.certificate, decoded.certificate);
        assert_eq!(cert.private_key, decoded.private_key);
    }

    #[test]
    fn decode_empty_input_fails() {
        // Empty input has not even a length prefix.
        assert!(decode(Vec::new()).is_err());
    }

    #[test]
    fn decode_too_few_bytes_for_length_prefix_fails() {
        // Not enough for the first length prefix.
        assert!(decode(vec![0; 4]).is_err());
    }

    #[test]
    fn decode_certificate_length_exceeds_buffer_fails() {
        // Certificate length bigger than buffer.
        let mut bytes = 16u64.to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0u8; 2]);
        assert!(decode(bytes).is_err());
    }

    #[test]
    fn decode_truncated_private_key_prefix_fails() {
        // too few bytes for the private-key length prefix.
        let mut bytes = 16u64.to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0u8; 16]); // certificate
        bytes.extend_from_slice(&[0u8; 3]);
        assert!(decode(bytes).is_err());
    }

    #[test]
    fn decode_private_key_length_exceeds_buffer_fails() {
        // Private-key length bigger than buffer.
        let mut bytes = 16u64.to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0u8; 16]); // certificate

        bytes.extend_from_slice(&16u64.to_be_bytes());
        bytes.extend_from_slice(&[0u8; 2]);
        assert!(decode(bytes).is_err());
    }

    #[test]
    fn support_trailing_bytes() {
        // generate -> encode -> extend -> decode
        let cert = generate_certificate().unwrap();
        let mut encoded = encode(&cert);
        encoded.extend_from_slice(&[7; 19]);
        let decoded = decode(encoded).unwrap();
        assert_eq!(cert.certificate, decoded.certificate);
        assert_eq!(cert.private_key, decoded.private_key);
    }

    #[test]
    fn validate_valid_certificate() {
        let cert = generate_certificate().unwrap();
        assert!(validate_certificate(&cert).is_ok());
    }

    #[test]
    fn validate_invalid_certificate() {
        let mut cert = generate_certificate().unwrap();
        *cert.certificate.last_mut().unwrap() = 0;
        *cert.private_key.last_mut().unwrap() = 0;
        assert!(validate_certificate(&cert).is_err());
    }
}
