// // Copyright 2023 litep2p developers
// //
// // Permission is hereby granted, free of charge, to any person obtaining a
// // copy of this software and associated documentation files (the "Software"),
// // to deal in the Software without restriction, including without limitation
// // the rights to use, copy, modify, merge, publish, distribute, sublicense,
// // and/or sell copies of the Software, and to permit persons to whom the
// // Software is furnished to do so, subject to the following conditions:
// //
// // The above copyright notice and this permission notice shall be included in
// // all copies or substantial portions of the Software.
// //
// // THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// // OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// // FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// // AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// // LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// // FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// // DEALINGS IN THE SOFTWARE.

// use litep2p::{
//     config::ConfigBuilder,
//     protocol::request_response::types::{Config as RequestResponseConfig, RequestResponseHandle},
//     transport::websocket::config::Config as WebSocketConfig,
//     Litep2p,
// };

// async fn initialize_litep2p() -> (Litep2p, RequestResponseHandle) {
//     let (config, handle) = RequestResponseConfig::new(
//         litep2p::types::protocol::ProtocolName::from("/request/1"),
//         64,
//     );

//     let litep2p = Litep2p::new(
//         ConfigBuilder::new()
//             .with_websocket(WebSocketConfig {
//                 listen_address: "/ip4/127.0.0.1/tcp/8888/ws".parse().unwrap(),
//             })
//             .with_request_response_protocol(config)
//             .build(),
//     )
//     .await
//     .unwrap();

//     (litep2p, handle)
// }

// #[tokio::test]
// async fn websocket_works() {
//     let _ = tracing_subscriber::fmt()
//         .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
//         .try_init();

//     let (mut litep2p, _handle) = initialize_litep2p().await;

//     tracing::info!("local peer id {}", litep2p.local_peer_id());

//     litep2p
//         .connect("/ip4/127.0.0.1/tcp/30333/ws".parse().unwrap())
//         .await
//         .unwrap();

//     loop {
//         tokio::select! {
//             event = litep2p.next_event() => {
//                 tracing::info!("litep2p event: {event:?}");
//             }
//             // event = handle.next_event() => {
//             //     tracing::info!("request-response event: {event:?}");
//             // }
//         }
//     }
// }
