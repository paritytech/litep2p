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
    crypto::{ed25519::Keypair, PublicKey},
    error::Error,
    new_config::Litep2pConfiguration,
    peer_id::PeerId,
    protocol::{
        libp2p::{identify, ping, Libp2pProtocolEvent},
        notification::NotificationProtocolConfig,
        request_response::RequestResponseProtocolConfig,
    },
    transport::{tcp::TcpTransport, Connection, Transport, TransportEvent},
    types::{ConnectionId, ProtocolId, RequestId},
    LOG_TARGET,
};

use futures::{Stream, StreamExt};
use identify::IdentifyEvent;
use multiaddr::{Multiaddr, Protocol};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};
use tokio_stream::{wrappers::ReceiverStream, StreamMap};

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
};

/// [`Litep2p`] object.
pub struct Litep2p {
    /// Local peer ID.
    local_peer_id: PeerId,

    // Litep2p configuration.
    config: Litep2pConfiguration,
}

impl Litep2p {
    /// Create new [`Litep2p`].
    pub async fn new(config: Litep2pConfiguration) -> crate::Result<Litep2p> {
        let local_peer_id = PeerId::from_public_key(&PublicKey::Ed25519(
            config.keypair.clone().expect("keypair to exist").public(),
        ));

        // enable tcp transport if the config exists
        if let Some(config) = config.tcp {
            todo!();
        }

        // TODO: how to store connections?

        Ok(Self {
            local_peer_id,
            config,
        })
    }

    /// Get local peer ID.
    pub fn local_peer_id(&self) -> &PeerId {
        &self.local_peer_id
    }
}
