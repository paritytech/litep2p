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

//! Connection-related helper code.

use crate::{
    error::Error,
    protocol::protocol_set::ProtocolCommand,
    types::{protocol::ProtocolName, SubstreamId},
};

use tokio::sync::mpsc::{Sender, WeakSender};

/// Connection type, from the point of view of the protocol.
#[derive(Debug, Clone)]
enum ConnectionType {
    /// Connection is actively kept open.
    Active(Sender<ProtocolCommand>),

    /// Connection is considered inactive as far as the protocol is concerned
    /// and if no substreams are being opened and no protocol is interested in
    /// keeping the connection open, it will be closed.
    Inactive(WeakSender<ProtocolCommand>),
}

/// Type representing a handle to connection which allows protocols to communicate with the connection.
#[derive(Debug, Clone)]
pub struct ConnectionHandle {
    /// Connection type.
    connection: ConnectionType,
}

impl ConnectionHandle {
    /// Create new [`ConnectionHandle`].
    ///
    /// By default the connection is set as `Active` to give protocols time to open a substream if
    /// they wish.
    pub fn new(connection: Sender<ProtocolCommand>) -> Self {
        Self {
            connection: ConnectionType::Active(connection),
        }
    }

    /// Get active sender from the [`ConnectionHandle`] and then downgrade it to an inactive connection.
    ///
    /// This function is only called once when the connection is established to remote peer and that one
    /// time the connection type must be `Active`, unless there is a logic bug in `litep2p`.
    pub fn downgrade(&mut self) -> Self {
        let connection = match &self.connection {
            ConnectionType::Active(connection) => {
                let handle = Self::new(connection.clone());
                self.connection = ConnectionType::Inactive(connection.downgrade());

                handle
            }
            ConnectionType::Inactive(_) => {
                panic!("state mismatch: tried to downgrade an inactive connection")
            }
        };

        connection
    }

    /// Mark connection as closed.
    pub fn close(&mut self) {
        if let ConnectionType::Active(connection) = &self.connection {
            self.connection = ConnectionType::Inactive(connection.downgrade());
        }
    }

    /// Attempt to acquire permit which will keep the connection open for indefinite time.
    pub fn try_get_permit(&self) -> Option<Permit> {
        match &self.connection {
            ConnectionType::Active(active) => Some(Permit::new(active.clone())),
            ConnectionType::Inactive(inactive) => Some(Permit::new(inactive.upgrade()?)),
        }
    }

    /// Open substream to remote peer over `protocol` and send the acquired permit to the
    /// transport so it can be given to the opened substream.
    pub async fn open_substream(
        &mut self,
        protocol: ProtocolName,
        substream_id: SubstreamId,
        permit: Permit,
    ) -> crate::Result<()> {
        match &self.connection {
            ConnectionType::Active(active) => active.clone(),
            ConnectionType::Inactive(inactive) => {
                inactive.upgrade().ok_or(Error::ConnectionClosed)?
            }
        }
        .send(ProtocolCommand::OpenSubstream {
            protocol: protocol.clone(),
            substream_id,
            permit,
        })
        .await
        .map_err(From::from)
    }
}

/// Type which allows the connection to be kept open.
#[derive(Debug)]
pub struct Permit {
    /// Active connection.
    connection: Sender<ProtocolCommand>,
}

impl Permit {
    /// Create new [`Permit`] which allows the connection to be kept open.
    pub fn new(connection: Sender<ProtocolCommand>) -> Self {
        Self { connection }
    }

    /// Get mutable access to inner connection.
    pub fn inner(&mut self) -> &mut Sender<ProtocolCommand> {
        &mut self.connection
    }
}
