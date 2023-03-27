use litep2p::{config::LiteP2pConfiguration, Litep2p};

#[tokio::test]
async fn libp2p() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .expect("to succeed");

    let mut litep2p = Litep2p::new(LiteP2pConfiguration::new(vec!["/ip6/::1/tcp/7777"
        .parse()
        .expect("valid multiaddress")]))
    .await
    .unwrap();

    litep2p
        .open_connection("/ip6/::1/tcp/8888".parse().expect("valid multiaddress"))
        .await;

    loop {
        tokio::select! {
            event = litep2p.next_event() => {
                tracing::info!("{event:?}");
            },
            _ = tokio::time::sleep(std::time::Duration::from_secs(3)) => {
                break
            }
        }
    }
}
