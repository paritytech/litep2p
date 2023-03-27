use litep2p::{config::LiteP2pConfiguration, Litep2p};

use futures::StreamExt;

#[tokio::test]
async fn libp2p() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .expect("to succeed");

    let (mut handle, mut event_stream) =
        Litep2p::new(LiteP2pConfiguration::new(vec!["/ip6/::1/tcp/7777"
            .parse()
            .expect("valid multiaddress")]))
        .await
        .unwrap();

    handle
        .open_connection("/ip6/::1/tcp/8888".parse().expect("valid multiaddress"))
        .await;

    if let Some(event) = event_stream.next().await {
        tracing::info!("{event:?}");
    } else {
        panic!("failed");
    }

    if let Some(event) = event_stream.next().await {
        tracing::info!("{event:?}");
    } else {
        panic!("failed");
    }

    // let litep2p = Litep2p::new(LiteP2pConfiguration::new(vec!["/ip6/::1/tcp/7777"
    //         .parse()
    //         .expect("valid multiaddress")]))
    //     .await
    //     .unwrap();

    // ProtocolConfig {
    //     tx: (), // for sending substream information to litep2p
    //     rx: (), // for receiving substream information from litep2p
    // }

    // let handle = litep2p.handle();

    // while let Some(event) = litep2p.next_event().await {
    //     todo!();
    // }

    // handle.open_connection(address);
    // handle.open_substream(protocol, )

    // let (_handle2, _event_stream2) =
    //     Litep2p::new(LiteP2pConfiguration::new(vec!["/ip6/::1/tcp/8889"
    //         .parse()
    //         .expect("valid multiaddress")]))
    //     .await
    //     .unwrap();
}
