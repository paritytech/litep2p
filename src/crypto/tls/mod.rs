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
#![allow(unused)]

// TODO: remove all unneeded stuff

use crate::{crypto::ed25519::Keypair, peer_id::PeerId};
use rustls::{ClientConfig, ServerConfig};
use s2n_quic::provider::tls::{
    rustls::{Client as TlsClient, Server as TlsServer},
    Provider,
};
use s2n_quic::{client::Connect, Client, Server};
use tokio::sync::mpsc::{channel, Sender};

use std::{error::Error, net::SocketAddr, path::Path, sync::Arc};

pub mod certificate;
mod upgrade;
mod verifier;

// TODO: remove maybe
pub use futures_rustls::TlsStream;
pub use upgrade::UpgradeError;

const P2P_ALPN: [u8; 6] = *b"libp2p";

pub(crate) struct TlsProvider {
    /// Private key.
    private_key: rustls::PrivateKey,

    /// Certificate.
    certificate: rustls::Certificate,

    /// Remote peer ID, provided for the TLS client.
    remote_peer_id: Option<PeerId>,

    /// Sender for the peer ID.
    sender: Option<Sender<PeerId>>,
}

impl TlsProvider {
    /// Create new [`TlsProvider`].
    pub(crate) fn new(
        private_key: rustls::PrivateKey,
        certificate: rustls::Certificate,
        remote_peer_id: Option<PeerId>,
        sender: Option<Sender<PeerId>>,
    ) -> Self {
        Self {
            sender,
            private_key,
            certificate,
            remote_peer_id,
        }
    }
}

impl Provider for TlsProvider {
    type Server = TlsServer;
    type Client = TlsClient;
    type Error = rustls::Error;

    fn start_server(self) -> Result<Self::Server, Self::Error> {
        let mut cfg = ServerConfig::builder()
            .with_cipher_suites(verifier::CIPHERSUITES)
            .with_safe_default_kx_groups()
            .with_protocol_versions(verifier::PROTOCOL_VERSIONS)
            .expect("Cipher suites and kx groups are configured; qed")
            .with_client_cert_verifier(Arc::new(verifier::Libp2pCertificateVerifier::with_sender(
                self.sender,
            )))
            .with_single_cert(vec![self.certificate], self.private_key)
            .expect("Server cert key DER is valid; qed");

        cfg.alpn_protocols = vec![P2P_ALPN.to_vec()];
        Ok(cfg.into())
    }

    fn start_client(self) -> Result<Self::Client, Self::Error> {
        let mut cfg = ClientConfig::builder()
            .with_cipher_suites(verifier::CIPHERSUITES)
            .with_safe_default_kx_groups()
            .with_protocol_versions(verifier::PROTOCOL_VERSIONS)
            .expect("Cipher suites and kx groups are configured; qed")
            .with_custom_certificate_verifier(Arc::new(
                verifier::Libp2pCertificateVerifier::with_remote_peer_id(self.remote_peer_id),
            ))
            .with_single_cert(vec![self.certificate], self.private_key)
            .expect("Client cert key DER is valid; qed");

        cfg.alpn_protocols = vec![P2P_ALPN.to_vec()];
        Ok(cfg.into())
    }
}

// TODO: remove
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::ed25519::Keypair;
    use rustls::{ClientConfig, ServerConfig};
    use s2n_quic::provider::tls::{
        rustls::{Client as TlsClient, Server as TlsServer},
        Provider,
    };
    use s2n_quic::{client::Connect, Client, Server};
    use std::{error::Error, net::SocketAddr, path::Path};

    // #[tokio::test]
    // async fn test_quic_server() {
    //     let _ = tracing_subscriber::fmt()
    //         .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
    //         .try_init();

    //     let keypair = Keypair::generate();
    //     let (certificate, key) = certificate::generate(&keypair).unwrap();

    //     let (tx, rx) = channel(1);
    //     let provider = TlsProvider::new(key, certificate, None, Some(tx));
    //     let mut server = Server::builder()
    //         .with_tls(provider)
    //         .unwrap()
    //         .with_io("127.0.0.1:4433")
    //         .unwrap()
    //         .start()
    //         .unwrap();

    //     while let Some(mut connection) = server.accept().await {
    //         tracing::info!("start quic server");
    //         // spawn a new task for the connection
    //         tokio::spawn(async move {
    //             while let Ok(Some(mut stream)) = connection.accept_bidirectional_stream().await {
    //                 tracing::info!("accept bidirectional stream");

    //                 // spawn a new task for the stream
    //                 tokio::spawn(async move {
    //                     // echo any data back to the stream
    //                     while let Ok(Some(data)) = stream.receive().await {
    //                         stream.send(data).await.expect("stream should be open");
    //                     }
    //                 });
    //             }
    //         });
    //     }
    // }

    // #[tokio::test]
    // async fn test_quic_client() {
    //     let _ = tracing_subscriber::fmt()
    //         .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
    //         .try_init();

    //     let keypair = Keypair::generate();
    //     let (certificate, key) = certificate::generate(&keypair).unwrap();
    //     let remote_peer_id = Some(
    //         PeerId::try_from_multiaddr(
    //             &multiaddr::Multiaddr::try_from(
    //                 "/p2p/12D3KooWLwag8ykamnDdwAQvTMx3i1i4BakdZqzfzvJm3t4knSda",
    //             )
    //             .unwrap(),
    //         )
    //         .unwrap(),
    //     );

    //     let provider = TlsProvider::new(key, certificate, remote_peer_id, None);
    //     let mut client = Client::builder()
    //         .with_tls(provider)
    //         .unwrap()
    //         .with_io("127.0.0.1:4433")
    //         .unwrap()
    //         .start()
    //         .unwrap();

    //     let addr: SocketAddr = "127.0.0.1:8888".parse().unwrap();
    //     let connect = Connect::new(addr).with_server_name("localhost");
    //     let mut connection = client.connect(connect).await.unwrap();

    //     // ensure the connection doesn't time out with inactivity
    //     connection.keep_alive(true).unwrap();

    //     // open a new stream and split the receiving and sending sides
    //     let stream = connection.open_bidirectional_stream().await.unwrap();
    //     let (mut receive_stream, mut send_stream) = stream.split();

    //     // spawn a task that copies responses from the server to stdout
    //     tokio::spawn(async move {
    //         let mut stdout = tokio::io::stdout();
    //         let _ = tokio::io::copy(&mut receive_stream, &mut stdout).await;
    //     });

    //     // copy data from stdin and send it to the server
    //     let mut stdin = tokio::io::stdin();
    //     tokio::io::copy(&mut stdin, &mut send_stream).await.unwrap();
    // }
}
