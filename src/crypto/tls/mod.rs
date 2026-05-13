// Copyright 2021 Parity Technologies (UK) Ltd.
// Copyright 2022 Protocol Labs.
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

//! TLS configuration based on libp2p TLS specs.
//!
//! See <https://github.com/libp2p/specs/blob/master/tls/tls.md>.

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use crate::{crypto::ed25519::Keypair, PeerId};

use std::sync::Arc;

pub mod certificate;
mod verifier;

const P2P_ALPN: [u8; 6] = *b"libp2p";

/// `CryptoProvider` narrowed to the libp2p TLS 1.3 cipher-suite and kx-group set.
///
/// `kx_groups` is pinned rather than inherited from the ring default so that a
/// future ring release can't silently widen the negotiation surface.
fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    use rustls::crypto::ring::kx_group;

    let mut provider = rustls::crypto::ring::default_provider();
    provider.cipher_suites = verifier::CIPHERSUITES.to_vec();
    provider.kx_groups = vec![kx_group::X25519, kx_group::SECP256R1, kx_group::SECP384R1];
    Arc::new(provider)
}

/// Create a TLS server configuration for litep2p.
pub fn make_server_config(
    keypair: &Keypair,
) -> Result<rustls::ServerConfig, certificate::GenError> {
    let (certificate, private_key) = certificate::generate(keypair)?;

    // Custom `ResolvesServerCert` bypasses `with_single_cert`'s validation
    // path; rustls 0.23 rejects our libp2p extension as an unsupported
    // critical X.509 extension if it goes through the standard path.
    let cert_resolver = Arc::new(
        certificate::AlwaysResolvesCert::new(certificate, &private_key)
            .expect("Server cert key DER is valid; qed"),
    );

    let mut crypto = rustls::ServerConfig::builder_with_provider(crypto_provider())
        .with_protocol_versions(verifier::PROTOCOL_VERSIONS)
        .expect("Cipher suites and kx groups are configured; qed")
        .with_client_cert_verifier(Arc::new(verifier::Libp2pCertificateVerifier::new()))
        .with_cert_resolver(cert_resolver);
    crypto.alpn_protocols = vec![P2P_ALPN.to_vec()];

    Ok(crypto)
}

/// Create a TLS client configuration for libp2p.
pub fn make_client_config(
    keypair: &Keypair,
    remote_peer_id: Option<PeerId>,
) -> Result<rustls::ClientConfig, certificate::GenError> {
    let (certificate, private_key) = certificate::generate(keypair)?;

    let cert_resolver = Arc::new(
        certificate::AlwaysResolvesCert::new(certificate, &private_key)
            .expect("Client cert key DER is valid; qed"),
    );

    let mut crypto = rustls::ClientConfig::builder_with_provider(crypto_provider())
        .with_protocol_versions(verifier::PROTOCOL_VERSIONS)
        .expect("Cipher suites and kx groups are configured; qed")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(
            verifier::Libp2pCertificateVerifier::with_remote_peer_id(remote_peer_id),
        ))
        .with_client_cert_resolver(cert_resolver);
    crypto.alpn_protocols = vec![P2P_ALPN.to_vec()];

    Ok(crypto)
}
