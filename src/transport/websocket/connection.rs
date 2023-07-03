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

#![allow(unused)]

use crate::{
    config::Role,
    crypto::noise::{self, Encrypted, NoiseConfiguration},
    multistream_select::{dialer_select_proto, listener_select_proto, Negotiated, Version},
    protocol::ProtocolSet,
    transport::TransportContext,
    types::{protocol::ProtocolName, ConnectionId},
};

use futures::{stream::TryStreamExt, AsyncRead, AsyncWrite, Stream, StreamExt};
use tokio::{io::AsyncReadExt, net::TcpStream};
use tokio_tungstenite::{accept_async, tungstenite::protocol::Message, WebSocketStream};
use tokio_util::io::StreamReader;
use tokio_util::{
    compat::{
        Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt,
    },
    io::SinkWriter,
};

use std::{
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

/// Logging target for the file.
const LOG_TARGET: &str = "websocket::connection";

/// WebSocket connection.
pub(crate) struct WebSocketConnection {}

impl WebSocketConnection {
    /// Negotiate protocol.
    async fn negotiate_protocol<S: AsyncRead + AsyncWrite + Unpin>(
        stream: S,
        role: &Role,
        protocols: Vec<&str>,
    ) -> crate::Result<(Negotiated<S>, ProtocolName)> {
        tracing::trace!(target: LOG_TARGET, ?protocols, "negotiating protocols");

        let (protocol, socket) = match role {
            Role::Dialer => dialer_select_proto(stream, protocols, Version::V1).await?,
            Role::Listener => listener_select_proto(stream, protocols).await?,
        };

        tracing::trace!(target: LOG_TARGET, ?protocol, "protocol negotiated");

        Ok((socket, ProtocolName::from(protocol.to_string())))
    }

    /// Negotiate noise + yamux for the connection.
    async fn negotiate_connection(
        stream: TcpStream,
        connection_id: ConnectionId,
        context: TransportContext,
        noise_config: NoiseConfiguration,
        address: SocketAddr,
    ) -> crate::Result<Self> {
        tracing::trace!(
            target: LOG_TARGET,
            role = ?noise_config.role,
            "negotiate connection",
        );

        let stream = TokioAsyncReadCompatExt::compat(stream).into_inner();
        let stream = TokioAsyncWriteCompatExt::compat_write(stream);

        let role = noise_config.role;
        let noise_config = NoiseConfiguration::new(&context.keypair, Role::Listener);

        // negotiate `noise`
        let (stream, _) =
            Self::negotiate_protocol(stream, &noise_config.role, vec!["/noise"]).await?;

        tracing::trace!(
            target: LOG_TARGET,
            "`multistream-select` and `noise` negotiated"
        );

        // perform noise handshake
        let (stream, peer) = noise::handshake(stream.inner(), noise_config).await?;
        tracing::trace!(target: LOG_TARGET, "noise handshake done");
        let stream: Encrypted<Compat<TcpStream>> = stream;

        // negotiate `yamux`
        let (stream, _) = Self::negotiate_protocol(stream, &role, vec!["/yamux/1.0.0"]).await?;
        tracing::trace!(target: LOG_TARGET, "`yamux` negotiated");

        let connection =
            yamux::Connection::new(stream.inner(), yamux::Config::default(), role.into());
        let (control, connection) = yamux::Control::new(connection);
        let context = ProtocolSet::from_transport_context(peer, context).await?;

        todo!();
        // Ok(Self {
        //     peer,
        //     context,
        //     control,
        //     connection,
        //     _connection_id: connection_id,
        //     next_substream_id: 0usize,
        //     pending_substreams: FuturesUnordered::new(),
        //     address: socket_addr_to_multi_addr(&address),
        // })
    }

    /// Accept WebSocket connection.
    pub(crate) async fn accept_connection(
        stream: TcpStream,
        address: SocketAddr,
        context: TransportContext,
    ) -> crate::Result<()> {
        let mut stream = tokio_tungstenite::accept_async(stream).await?;

        tracing::error!(
            target: LOG_TARGET,
            ?address,
            "connection received, negotiate protocols"
        );

        while let Some(value) = stream.next().await {
            tracing::info!(target: LOG_TARGET, "read: {value:?}");
        }

        // let stream = TokioAsyncReadCompatExt::compat(ws_stream).into_inner();
        // let stream = TokioAsyncWriteCompatExt::compat_write(stream);
        // let stream = stream.into_async_read();
        // let stream =
        // let noise_config = NoiseConfiguration::new(&context.keypair, Role::Listener);
        // let role = noise_config.role;

        // // negotiate `noise`
        // let (stream, _) =
        //     Self::negotiate_protocol(stream, &noise_config.role, vec!["/noise"]).await?;

        // tracing::trace!(
        //     target: LOG_TARGET,
        //     "`multistream-select` and `noise` negotiated"
        // );

        // while let Some(event) = ws_stream.next().await {
        //     tracing::info!(target: LOG_TARGET, ?event, "event received")
        // }

        todo!();
    }
}
