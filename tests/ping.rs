use litep2p::{config::LiteP2pConfiguration, Litep2p};

#[tokio::test]
async fn ping() {
    let _litep2p1 = Litep2p::new(LiteP2pConfiguration::new(vec!["/ip6/::1/tcp/8888"
        .parse()
        .expect("valid multiaddress")]))
    .await
    .unwrap();
    let _litep2p2 = Litep2p::new(LiteP2pConfiguration::new(vec!["/ip6/::1/tcp/8889"
        .parse()
        .expect("valid multiaddress")]))
    .await
    .unwrap();
}
