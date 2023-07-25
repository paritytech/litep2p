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
    codec::unsigned_varint::UnsignedVarint,
    config::Role,
    crypto::{
        noise::{self, Encrypted, NoiseConfiguration, STATIC_KEY_DOMAIN},
        PublicKey,
    },
    error::{Error, NegotiationError},
    multistream_select::{Message as MultiStreamMessage, Protocol},
    peer_id::PeerId,
    protocol::ProtocolSet,
    transport::TransportContext,
    types::{protocol::ProtocolName, ConnectionId},
};

use bytes::BytesMut;
use futures::{stream::TryStreamExt, AsyncRead, AsyncWrite, SinkExt, Stream, StreamExt};
use prost::Message as _;
use tokio::{io::AsyncReadExt, net::TcpStream};
use tokio_tungstenite::{accept_async, tungstenite::protocol::Message, WebSocketStream};
use tokio_util::{compat::FuturesAsyncWriteCompatExt, io::StreamReader};
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

mod schema {
    pub(super) mod noise {
        include!(concat!(env!("OUT_DIR"), "/noise.rs"));
    }
}

/// Logging target for the file.
const LOG_TARGET: &str = "websocket::connection";

/// WebSocket connection.
pub(crate) struct WebSocketConnection {}

impl WebSocketConnection {
    /// Negotiate protocol.
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

        // negotiate noise
        loop {
            match stream.next().await {
                Some(Ok(Message::Binary(value))) => {
                    tracing::info!(
                        target: LOG_TARGET,
                        "decoded: {:?}",
                        std::str::from_utf8(&value)
                    );
                    match MultiStreamMessage::decode(value.into()) {
                        Ok(MultiStreamMessage::Protocols(protocols)) => {
                            if protocols.len() != 2 {
                                todo!();
                            }

                            let multistream = ProtocolName::from("/multistream/1.0.0");
                            let noise = ProtocolName::from("/noise");

                            if protocols[0].as_ref() == multistream.as_bytes() {
                                if protocols[1].as_ref() == noise.as_bytes() {
                                    tracing::error!(target: LOG_TARGET, "VALID PROTOCOL");

                                    let mut bytes = BytesMut::with_capacity(128);
                                    let message = MultiStreamMessage::Header(
                                        crate::multistream_select::HeaderLine::V1,
                                    );
                                    let _ = message.encode(&mut bytes).unwrap();

                                    // TODO: no unwraps
                                    let mut header = UnsignedVarint::encode(bytes).unwrap();

                                    let mut proto_bytes = BytesMut::with_capacity(128);
                                    let message =
                                        MultiStreamMessage::Protocol(protocols[1].clone());
                                    let _ = message.encode(&mut proto_bytes).unwrap();
                                    let proto_bytes = UnsignedVarint::encode(proto_bytes).unwrap();

                                    header.append(&mut proto_bytes.into());

                                    stream.send(Message::Binary(header)).await.unwrap();
                                    break;
                                }
                            }

                            tracing::info!(
                                target: LOG_TARGET,
                                ?protocols,
                                "respond to multistream-select"
                            );
                        }
                        event => tracing::warn!(target: LOG_TARGET, "unhandled event {event:?}"),
                    }
                }
                event => todo!("unhandled event 2 {event:?}"),
            }
        }

        tracing::info!(target: LOG_TARGET, "noise negotiated");

        let noise = snow::Builder::new("Noise_XX_25519_ChaChaPoly_SHA256".parse().unwrap());
        let keypair = noise.generate_keypair().unwrap();
        let mut noise = noise
            .local_private_key(&keypair.private)
            .build_responder()
            .unwrap();

        let noise_payload = schema::noise::NoiseHandshakePayload {
            identity_key: Some(PublicKey::Ed25519(context.keypair.public()).to_protobuf_encoding()),
            identity_sig: Some(
                context
                    .keypair
                    .sign(&[STATIC_KEY_DOMAIN.as_bytes(), keypair.public.as_ref()].concat()),
            ),
            ..Default::default()
        };
        let mut payload = Vec::with_capacity(noise_payload.encoded_len());
        noise_payload
            .encode(&mut payload)
            .expect("Vec<u8> provides capacity as needed");

        // TODO: ignore random message
        let _ = stream.next().await;

        loop {
            match stream.next().await {
                Some(Ok(Message::Binary(message))) => {
                    tracing::info!(target: LOG_TARGET, "message len {}", message.len());
                    let mut buffer = vec![0u8; 1024];

                    match noise.read_message(&message, &mut buffer) {
                        Ok(len) => buffer.truncate(len),
                        Err(error) => tracing::error!(
                            target: LOG_TARGET,
                            "failed to decode initial handshake"
                        ),
                    }

                    let mut buffer = vec![0u8; 1024];
                    let payload = match noise.write_message(&payload, &mut buffer) {
                        Ok(len) => {
                            buffer.truncate(len);
                            let size = len as u16;
                            let mut size = size.to_be_bytes().to_vec();
                            size.append(&mut buffer);

                            size
                        }
                        Err(error) => {
                            tracing::error!(
                                target: LOG_TARGET,
                                ?error,
                                "failed to write noise message"
                            );
                            todo!();
                        }
                    };

                    stream.send(Message::Binary(payload)).await.unwrap();

                    let _ = stream.next().await.unwrap();
                    let message = stream.next().await.unwrap();

                    match message {
                        Ok(Message::Binary(message)) => {
                            let mut buffer = vec![0u8; 1024];
                            match noise.read_message(&message, &mut buffer) {
                                Ok(len) => buffer.truncate(len),
                                Err(error) => tracing::error!(
                                    target: LOG_TARGET,
                                    ?error,
                                    "failed to read second noise message"
                                ),
                            }
                            let bytes: bytes::Bytes = buffer.into();

                            let peer_id = match schema::noise::NoiseHandshakePayload::decode(bytes)
                            {
                                Ok(payload) => {
                                    tracing::info!(target: LOG_TARGET, "got peer public key");
                                    let public_key = PublicKey::from_protobuf_encoding(
                                        &payload.identity_key.ok_or(Error::NegotiationError(
                                            NegotiationError::PeerIdMissing,
                                        ))?,
                                    )?;
                                    Ok(PeerId::from_public_key(&public_key))
                                }
                                Err(err) => {
                                    tracing::error!(
                                        target: LOG_TARGET,
                                        "failed to read public key"
                                    );
                                    Err(Error::Unknown)
                                }
                            };

                            tracing::info!(target: LOG_TARGET, "peer id, maybe: {peer_id:?}");
                        }
                        event => tracing::error!(target: LOG_TARGET, "unhandled event {event:?}"),
                    }

                    break;
                }
                event => todo!("unhandled event 3 {event:?}"),
            }
        }

        tracing::info!(target: LOG_TARGET, "NOISE FINISHED");
        let mut noise = noise.into_transport_mode().unwrap();

        let mut buffer = vec![0u8; 1024];

        // loop {
        match stream.next().await {
            Some(Ok(Message::Binary(message))) => {
                tracing::info!(target: LOG_TARGET, "read message len {}", message.len());
            }
            event => todo!("unhandled event 4 {event:?}"),
        }

        match stream.next().await {
            Some(Ok(Message::Binary(message))) => {
                match noise.read_message(&message, &mut buffer) {
                    Ok(len) => buffer.truncate(len),
                    Err(error) => {
                        tracing::error!(
                            target: LOG_TARGET,
                            ?error,
                            "failed to read noise message: {} {:?}",
                            message.len(),
                            std::str::from_utf8(&message),
                        );
                        // todo!();
                        // continue;
                    }
                }

                tracing::info!(
                    target: LOG_TARGET,
                    "message: {:?}",
                    std::str::from_utf8(&buffer),
                );

                let multistream = ProtocolName::from("/multistream/1.0.0");
                let noise = ProtocolName::from("/noise");

                match MultiStreamMessage::decode(message.into()) {
                    Ok(MultiStreamMessage::Protocols(protocols)) => {
                        let multistream = ProtocolName::from("/multistream/1.0.0");
                        let noise = ProtocolName::from("/yamux/1.0.0");

                        if protocols[0].as_ref() == multistream.as_bytes() {
                            if protocols[1].as_ref() == noise.as_bytes() {
                                tracing::error!(target: LOG_TARGET, "VALID PROTOCOL");

                                let mut bytes = BytesMut::with_capacity(128);
                                let message = MultiStreamMessage::Header(
                                    crate::multistream_select::HeaderLine::V1,
                                );
                                let _ = message.encode(&mut bytes).unwrap();

                                // TODO: no unwraps
                                let mut header = UnsignedVarint::encode(bytes).unwrap();

                                let mut proto_bytes = BytesMut::with_capacity(128);
                                let message = MultiStreamMessage::Protocol(protocols[1].clone());
                                let _ = message.encode(&mut proto_bytes).unwrap();
                                let proto_bytes = UnsignedVarint::encode(proto_bytes).unwrap();

                                header.append(&mut proto_bytes.into());

                                stream.send(Message::Binary(header)).await.unwrap();
                                // break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            event => todo!("unhandled event 4 {event:?}"),
        }

        tracing::info!(target: LOG_TARGET, "YAMUX NEGOTIATED");
        // }
        todo!();
    }
}
