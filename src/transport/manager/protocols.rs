// Copyright 2026 litep2p developers
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
    protocol::{ConnectionHandle, InnerTransportEvent, SubstreamKeepAlive},
    transport::{
        manager::{InboundProtocol, ProtocolContext},
        Endpoint,
    },
    types::{protocol::ProtocolName, ConnectionId},
    PeerId,
};

use parking_lot::RwLock;
use tokio::sync::mpsc::Sender;

use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

/// Installed protocols shared between the transport manager and the open connections.
///
/// Established connections are tracked under the same lock, so every protocol is told about a
/// connection exactly once, by the connection or when it's installed, and is told about the
/// connection closing only if it was told about the connection.
#[derive(Debug, Clone, Default)]
pub struct ProtocolRegistry(Arc<RwLock<Protocols>>);

/// [`ProtocolRegistry`] that doesn't keep the senders of the installed protocols alive.
#[derive(Debug, Clone, Default)]
pub struct WeakProtocolRegistry(Weak<RwLock<Protocols>>);

impl WeakProtocolRegistry {
    pub fn upgrade(&self) -> Option<ProtocolRegistry> {
        self.0.upgrade().map(ProtocolRegistry)
    }
}

#[derive(Debug, Default)]
struct Protocols {
    contexts: HashMap<ProtocolName, ProtocolContext>,
    /// Maps a fallback name to the main protocol name.
    fallback_names: HashMap<ProtocolName, ProtocolName>,
    keep_alives: Arc<HashMap<ProtocolName, SubstreamKeepAlive>>,
    connections: HashMap<ConnectionId, (PeerId, Endpoint, ConnectionHandle)>,
}

impl Protocols {
    fn new(contexts: HashMap<ProtocolName, ProtocolContext>) -> Self {
        let fallback_names = contexts
            .iter()
            .flat_map(|(protocol, context)| {
                context
                    .fallback_names
                    .iter()
                    .map(|fallback| (fallback.clone(), protocol.clone()))
            })
            .collect();

        let keep_alives = contexts
            .iter()
            .filter(|(_, context)| context.inbound == InboundProtocol::Accept)
            .flat_map(|(protocol, context)| {
                std::iter::once(protocol)
                    .chain(&context.fallback_names)
                    .map(|name| (name.clone(), context.keep_alive))
            })
            .collect();

        Self {
            contexts,
            fallback_names,
            keep_alives: Arc::new(keep_alives),
            connections: HashMap::new(),
        }
    }
}

impl From<HashMap<ProtocolName, ProtocolContext>> for ProtocolRegistry {
    fn from(protocols: HashMap<ProtocolName, ProtocolContext>) -> Self {
        Self(Arc::new(RwLock::new(Protocols::new(protocols))))
    }
}

impl ProtocolRegistry {
    pub fn downgrade(&self) -> WeakProtocolRegistry {
        WeakProtocolRegistry(Arc::downgrade(&self.0))
    }

    fn rebuild<R>(
        protocols: &mut Protocols,
        f: impl FnOnce(&mut HashMap<ProtocolName, ProtocolContext>) -> R,
    ) -> R {
        let mut contexts = std::mem::take(&mut protocols.contexts);
        let updated = f(&mut contexts);
        let connections = std::mem::take(&mut protocols.connections);
        *protocols = Protocols::new(contexts);
        protocols.connections = connections;
        updated
    }

    /// Install `protocol` and return the established connections it must be told about.
    pub fn insert(
        &self,
        protocol: ProtocolName,
        context: ProtocolContext,
    ) -> Vec<(PeerId, Endpoint, ConnectionHandle)> {
        let mut protocols = self.0.write();
        Self::rebuild(&mut protocols, |contexts| {
            contexts.insert(protocol, context)
        });
        protocols.connections.values().cloned().collect()
    }

    /// Track an established connection and return the protocols that must be told about it.
    pub fn add_connection(
        &self,
        peer: PeerId,
        endpoint: Endpoint,
        handle: ConnectionHandle,
    ) -> Vec<(ProtocolName, Sender<InnerTransportEvent>)> {
        let mut protocols = self.0.write();
        protocols.connections.insert(endpoint.connection_id(), (peer, endpoint, handle));
        Self::senders_of(&protocols)
    }

    /// Stop tracking a closed connection and return the protocols that must be told about it.
    pub fn remove_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Vec<(ProtocolName, Sender<InnerTransportEvent>)> {
        let mut protocols = self.0.write();
        match protocols.connections.remove(&connection_id) {
            Some(_) => Self::senders_of(&protocols),
            None => Vec::new(),
        }
    }

    /// Remove `protocol` if it's still installed with `tx`.
    pub fn remove(&self, protocol: &ProtocolName, tx: &Sender<InnerTransportEvent>) -> bool {
        Self::rebuild(&mut self.0.write(), |contexts| {
            contexts.get(protocol).is_some_and(|context| context.tx.same_channel(tx))
                && contexts.remove(protocol).is_some()
        })
    }

    /// Check if `name` is the main or a fallback name of an installed protocol.
    pub fn contains(&self, name: &ProtocolName) -> bool {
        self.get(name).is_some()
    }

    /// Get the main name and the context of the protocol that `name` belongs to.
    pub fn get(&self, name: &ProtocolName) -> Option<(ProtocolName, ProtocolContext)> {
        let protocols = self.0.read();

        protocols
            .contexts
            .get_key_value(protocols.fallback_names.get(name).unwrap_or(name))
            .map(|(protocol, context)| (protocol.clone(), context.clone()))
    }

    /// Main names of the protocols advertised to remote peers.
    pub fn advertised(&self) -> Vec<ProtocolName> {
        self.0
            .read()
            .contexts
            .iter()
            .filter(|(_, context)| context.inbound == InboundProtocol::Accept)
            .map(|(protocol, _)| protocol.clone())
            .collect()
    }

    /// Keep-alive settings of the main and fallback names that remote peers can negotiate.
    pub fn keep_alives(&self) -> Arc<HashMap<ProtocolName, SubstreamKeepAlive>> {
        self.0.read().keep_alives.clone()
    }

    pub fn senders(&self) -> Vec<(ProtocolName, Sender<InnerTransportEvent>)> {
        Self::senders_of(&self.0.read())
    }

    fn senders_of(protocols: &Protocols) -> Vec<(ProtocolName, Sender<InnerTransportEvent>)> {
        protocols
            .contexts
            .iter()
            .map(|(protocol, context)| (protocol.clone(), context.tx.clone()))
            .collect()
    }
}
