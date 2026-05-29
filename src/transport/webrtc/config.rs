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

//! WebRTC transport configuration.

use multiaddr::Multiaddr;
use std::path::PathBuf;

/// WebRTC transport configuration.
#[derive(Debug)]
pub struct Config {
    /// WebRTC listening address.
    pub listen_addresses: Vec<Multiaddr>,

    /// Connection datagram buffer size.
    ///
    /// How many datagrams can the buffer between `WebRtcTransport` and a connection handler hold.
    pub datagram_buffer_size: usize,

    /// Folder used to persist the DTLS certificate across restarts.
    ///
    /// If specified and a certificate file already exists in the folder, the
    /// certificate is loaded from it, otherwise a new certificate is generated
    /// and saved into the folder. Persisting it keeps the node's `certhash`
    /// (and therefore its advertised addresses) stable across restarts.
    ///
    /// NOTE: a folder must be used by a single node only, different nodes on the
    /// same machine must use different folders.
    pub dtdl_cert_persistent_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addresses: vec!["/ip4/127.0.0.1/udp/8888/webrtc-direct"
                .parse()
                .expect("valid multiaddress")],
            datagram_buffer_size: 2048,
            dtdl_cert_persistent_path: None,
        }
    }
}
