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

// use litep2p::{config::LiteP2pConfiguration, Litep2p};

// #[tokio::test]
// async fn libp2p() {
//     tracing_subscriber::fmt()
//         .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
//         .try_init()
//         .expect("to succeed");

//     let mut litep2p = Litep2p::new(LiteP2pConfiguration::new(
//         vec!["/ip6/::1/tcp/7777".parse().expect("valid multiaddress")],
//         vec![],
//         vec![],
//     ))
//     .await
//     .unwrap();

//     litep2p
//         .open_connection("/ip6/::1/tcp/8888".parse().expect("valid multiaddress"))
//         .await
//         .unwrap();

//     loop {
//         tokio::select! {
//             event = litep2p.next_event() => {
//                 tracing::info!("{event:?}");
//             },
//             _ = tokio::time::sleep(std::time::Duration::from_secs(3)) => {
//                 break
//             }
//         }
//     }
// }
