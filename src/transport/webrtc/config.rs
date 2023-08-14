// Copyright 2022 Parity Technologies (UK) Ltd.
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

use crate::{
    crypto::ed25519::Keypair,
    transport::webrtc::{certificate::Certificate, fingerprint::Fingerprint},
};

use webrtc::peer_connection::configuration::RTCConfiguration;

/// A config which holds peer's keys and a x509Cert used to authenticate WebRTC communications.
#[derive(Clone)]
pub struct Config {
    /// Inner WebRTC configuration.
    pub inner: RTCConfiguration,

    /// Local certificate fingerprint.
    pub fingerprint: Fingerprint,

    /// Keypair.
    pub id_keys: Keypair,
}

impl Config {
    /// Create new [`Config`].
    pub fn new(id_keys: Keypair, certificate: Certificate) -> Self {
        let fingerprint = certificate.fingerprint();

        Self {
            id_keys,
            inner: RTCConfiguration {
                certificates: vec![certificate.to_rtc_certificate()],
                ..RTCConfiguration::default()
            },
            fingerprint,
        }
    }
}
