// Copyright 2023 litep2p developers
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

use litep2p::{
    codec::ProtocolCodec,
    config::Litep2pConfigBuilder,
    crypto::ed25519::Keypair,
    protocol::{Transport, TransportEvent, TransportService, UserProtocol},
    transport::tcp::config::TransportConfig as TcpTransportConfig,
    types::protocol::ProtocolName,
    Litep2p, PeerId,
};

use std::{collections::HashSet, sync::Arc};

struct CustomProtocol {
    protocol: ProtocolName,
    codec: ProtocolCodec,
    peers: HashSet<PeerId>,
}

impl CustomProtocol {
    pub fn new() -> Self {
        let protocol: Arc<str> = Arc::from(String::from("/custom-protocol/1"));

        Self {
            peers: HashSet::new(),
            protocol: ProtocolName::from(protocol),
            codec: ProtocolCodec::UnsignedVarint(None),
        }
    }
}

#[async_trait::async_trait]
impl UserProtocol for CustomProtocol {
    fn protocol(&self) -> ProtocolName {
        self.protocol.clone()
    }

    fn codec(&self) -> ProtocolCodec {
        self.codec.clone()
    }

    async fn run(mut self: Box<Self>, mut service: TransportService) -> litep2p::Result<()> {
        loop {
            while let Some(event) = service.next_event().await {
                match event {
                    TransportEvent::ConnectionEstablished { peer, .. } => {
                        self.peers.insert(peer);
                    }
                    TransportEvent::ConnectionClosed { peer } => {
                        self.peers.remove(&peer);
                    }
                    _ => {}
                }
            }
        }
    }
}

#[tokio::test]
async fn user_protocol() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let custom_protocol1 = Box::new(CustomProtocol::new());
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            ..Default::default()
        })
        .with_user_protocol(custom_protocol1)
        .build();

    let custom_protocol2 = Box::new(CustomProtocol::new());
    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            ..Default::default()
        })
        .with_user_protocol(custom_protocol2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).unwrap();
    let mut litep2p2 = Litep2p::new(config2).unwrap();
    let address = litep2p2.listen_addresses().next().unwrap().clone();

    litep2p1.dial_address(address).await.unwrap();

    // wait until connection is established
    let mut litep2p1_ready = false;
    let mut litep2p2_ready = false;

    while !litep2p1_ready && !litep2p2_ready {
        tokio::select! {
            _event = litep2p1.next_event() => litep2p1_ready = true,
            _event = litep2p2.next_event() => litep2p2_ready = true,
        }
    }

    // wait until connection is closed by the keep-alive timeout
    let mut litep2p1_ready = false;
    let mut litep2p2_ready = false;

    while !litep2p1_ready && !litep2p2_ready {
        tokio::select! {
            _event = litep2p1.next_event() => litep2p1_ready = true,
            _event = litep2p2.next_event() => litep2p2_ready = true,
        }
    }
}
