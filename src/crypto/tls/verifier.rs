// Copyright 2021 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! TLS 1.3 certificates and handshakes handling for libp2p
//!
//! This module handles a verification of a client/server certificate chain
//! and signatures allegedly by the given certificates.

use crate::{crypto::tls::certificate, PeerId};

use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::ring::cipher_suite::{
        TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
    },
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
    CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme,
    SupportedCipherSuite, SupportedProtocolVersion,
};

/// The protocol versions supported by this verifier.
///
/// The spec says:
///
/// > The libp2p handshake uses TLS 1.3 (and higher).
/// > Endpoints MUST NOT negotiate lower TLS versions.
pub static PROTOCOL_VERSIONS: &[&SupportedProtocolVersion] = &[&rustls::version::TLS13];

/// A list of the TLS 1.3 cipher suites supported by rustls.
// By default rustls creates client/server configs with both
// TLS 1.3 __and__ 1.2 cipher suites. But we don't need 1.2.
pub static CIPHERSUITES: &[SupportedCipherSuite] = &[
    // TLS1.3 suites
    TLS13_CHACHA20_POLY1305_SHA256,
    TLS13_AES_256_GCM_SHA384,
    TLS13_AES_128_GCM_SHA256,
];

/// Implementation of the `rustls` certificate verification traits for libp2p.
///
/// Only TLS 1.3 is supported. TLS 1.2 should be disabled in the configuration of `rustls`.
#[derive(Debug)]
pub struct Libp2pCertificateVerifier {
    /// The peer ID we intend to connect to
    remote_peer_id: Option<PeerId>,
}

/// libp2p requires the following of X.509 server certificate chains:
///
/// - Exactly one certificate must be presented.
/// - The certificate must be self-signed.
/// - The certificate must have a valid libp2p extension that includes a signature of its public
///   key.
impl Libp2pCertificateVerifier {
    pub fn new() -> Self {
        Self {
            remote_peer_id: None,
        }
    }

    pub fn with_remote_peer_id(remote_peer_id: Option<PeerId>) -> Self {
        Self { remote_peer_id }
    }

    /// Return the list of SignatureSchemes that this verifier will handle,
    /// in `verify_tls12_signature` and `verify_tls13_signature` calls.
    ///
    /// This should be in priority order, with the most preferred first.
    fn verification_schemes() -> Vec<SignatureScheme> {
        vec![
            // TODO SignatureScheme::ECDSA_NISTP521_SHA512 is not supported by `ring` yet
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            // TODO SignatureScheme::ED448 is not supported by `ring` yet
            SignatureScheme::ED25519,
            // In particular, RSA SHOULD NOT be used unless
            // no elliptic curve algorithms are supported.
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA256,
        ]
    }
}

impl ServerCertVerifier for Libp2pCertificateVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let peer_id = verify_presented_certs(end_entity, intermediates)?;

        if let Some(remote_peer_id) = self.remote_peer_id {
            // The public host key allows the peer to calculate the peer ID of the peer
            // it is connecting to. Clients MUST verify that the peer ID derived from
            // the certificate matches the peer ID they intended to connect to,
            // and MUST abort the connection if there is a mismatch.
            if remote_peer_id != peer_id {
                // Ships a `bad_certificate` / `access_denied` TLS alert;
                // `Error::General` would emit the less specific `internal_error`.
                return Err(rustls::Error::InvalidCertificate(
                    CertificateError::ApplicationVerificationFailure,
                ));
            }
        }

        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        unreachable!("`PROTOCOL_VERSIONS` only allows TLS 1.3")
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(cert, dss.scheme, message, dss.signature())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        Self::verification_schemes()
    }
}

/// libp2p requires the following of X.509 client certificate chains:
///
/// - Exactly one certificate must be presented. In particular, client authentication is mandatory
///   in libp2p.
/// - The certificate must be self-signed.
/// - The certificate must have a valid libp2p extension that includes a signature of its public
///   key.
impl ClientCertVerifier for Libp2pCertificateVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // libp2p does not authenticate via PKI / root CAs; the cert is self-signed and
        // identity is bound through the libp2p extension. No subject hints to offer.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let _: PeerId = verify_presented_certs(end_entity, intermediates)?;

        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        unreachable!("`PROTOCOL_VERSIONS` only allows TLS 1.3")
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(cert, dss.scheme, message, dss.signature())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        Self::verification_schemes()
    }
}

/// When receiving the certificate chain, an endpoint
/// MUST check these conditions and abort the connection attempt if
/// (a) the presented certificate is not yet valid, OR
/// (b) if it is expired.
/// Endpoints MUST abort the connection attempt if more than one certificate is received,
/// or if the certificate’s self-signature is not valid.
fn verify_presented_certs(
    end_entity: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
) -> Result<PeerId, rustls::Error> {
    if !intermediates.is_empty() {
        return Err(rustls::Error::General(
            "libp2p-tls requires exactly one certificate".into(),
        ));
    }

    let cert = certificate::parse(end_entity)?;

    Ok(cert.peer_id())
}

fn verify_tls13_signature(
    cert: &CertificateDer<'_>,
    signature_scheme: SignatureScheme,
    message: &[u8],
    signature: &[u8],
) -> Result<HandshakeSignatureValid, rustls::Error> {
    certificate::parse(cert)?.verify_signature(signature_scheme, message, signature)?;

    Ok(HandshakeSignatureValid::assertion())
}

impl From<certificate::ParseError> for rustls::Error {
    fn from(certificate::ParseError(e): certificate::ParseError) -> Self {
        use webpki::Error::*;
        match e {
            BadDer => rustls::Error::InvalidCertificate(CertificateError::BadEncoding),
            e => rustls::Error::InvalidCertificate(CertificateError::Other(
                rustls::OtherError(std::sync::Arc::new(InvalidLibp2pCertificate(e))),
            )),
        }
    }
}
impl From<certificate::VerificationError> for rustls::Error {
    fn from(certificate::VerificationError(e): certificate::VerificationError) -> Self {
        use webpki::Error::*;
        match e {
            InvalidSignatureForPublicKey =>
                rustls::Error::InvalidCertificate(CertificateError::BadSignature),
            UnsupportedSignatureAlgorithm | UnsupportedSignatureAlgorithmForPublicKey =>
                rustls::Error::InvalidCertificate(
                    CertificateError::UnsupportedSignatureAlgorithm,
                ),
            e => rustls::Error::InvalidCertificate(CertificateError::Other(
                rustls::OtherError(std::sync::Arc::new(InvalidLibp2pCertificate(e))),
            )),
        }
    }
}

/// Wrapper to turn a `webpki::Error` into something that satisfies the bounds on
/// `rustls::CertificateError::Other`, which requires `std::error::Error + Send + Sync + 'static`.
#[derive(Debug)]
struct InvalidLibp2pCertificate(webpki::Error);

impl std::fmt::Display for InvalidLibp2pCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid peer certificate: {}", self.0)
    }
}

impl std::error::Error for InvalidLibp2pCertificate {}
