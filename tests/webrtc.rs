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

use futures::StreamExt;
use litep2p::{
    config::Litep2pConfigBuilder,
    crypto::ed25519::Keypair,
    protocol::{
        libp2p::ping::Config as PingConfig, notification::types::Config as NotificationConfig,
    },
    transport::webrtc::WebRtcTransportConfig,
    types::protocol::ProtocolName,
    Litep2p,
};

#[tokio::test]
async fn webrtc_test() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (ping_config, mut ping_event_stream) = PingConfig::new(3);
    let (notif_config, mut notif_event_stream) = NotificationConfig::new(
        ProtocolName::from(
            "/980e7cbafbcd37f8cb17be82bf8d53fa81c9a588e8a67384376e862da54285dc/block-announces/1",
        ),
        1024usize,
        vec![1, 2, 3, 4],
        Vec::new(),
    );

    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_webrtc(WebRtcTransportConfig {
            listen_address: "/ip4/192.168.1.173/udp/8888/webrtc-direct".parse().unwrap(),
        })
        .with_ipfs_ping(ping_config)
        .with_notification_protocol(notif_config)
        .build();

    let mut litep2p = Litep2p::new(config1).await.unwrap();
    let address = litep2p.listen_addresses().next().unwrap().clone();

    tracing::info!("listen address: {address:?}");

    loop {
        tokio::select! {
            event = litep2p.next_event() => {
                tracing::debug!("litep2p event received: {event:?}");
            }
            event = ping_event_stream.next() => {
                if std::matches!(event, None) {
                    tracing::error!("ping event stream termintated");
                    break
                }
                tracing::info!("ping event received: {event:?}");
            }
            event = notif_event_stream.next_event() => {
                tracing::error!("notification event received: {event:?}");
            }
        }
    }
}
