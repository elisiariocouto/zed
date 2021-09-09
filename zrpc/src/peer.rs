use super::proto::{self, AnyTypedEnvelope, EnvelopedMessage, MessageStream, RequestMessage};
use super::Conn;
use anyhow::{anyhow, Context, Result};
use async_lock::{Mutex, RwLock};
use futures::FutureExt as _;
use postage::{
    mpsc,
    prelude::{Sink as _, Stream as _},
};
use std::{
    collections::HashMap,
    fmt,
    future::Future,
    marker::PhantomData,
    sync::{
        atomic::{self, AtomicU32},
        Arc,
    },
};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ConnectionId(pub u32);

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PeerId(pub u32);

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

pub struct Receipt<T> {
    pub sender_id: ConnectionId,
    pub message_id: u32,
    payload_type: PhantomData<T>,
}

impl<T> Clone for Receipt<T> {
    fn clone(&self) -> Self {
        Self {
            sender_id: self.sender_id,
            message_id: self.message_id,
            payload_type: PhantomData,
        }
    }
}

impl<T> Copy for Receipt<T> {}

pub struct TypedEnvelope<T> {
    pub sender_id: ConnectionId,
    pub original_sender_id: Option<PeerId>,
    pub message_id: u32,
    pub payload: T,
}

impl<T> TypedEnvelope<T> {
    pub fn original_sender_id(&self) -> Result<PeerId> {
        self.original_sender_id
            .ok_or_else(|| anyhow!("missing original_sender_id"))
    }
}

impl<T: RequestMessage> TypedEnvelope<T> {
    pub fn receipt(&self) -> Receipt<T> {
        Receipt {
            sender_id: self.sender_id,
            message_id: self.message_id,
            payload_type: PhantomData,
        }
    }
}

pub struct Peer {
    connections: RwLock<HashMap<ConnectionId, Connection>>,
    next_connection_id: AtomicU32,
}

#[derive(Clone)]
struct Connection {
    outgoing_tx: mpsc::Sender<proto::Envelope>,
    next_message_id: Arc<AtomicU32>,
    response_channels: Arc<Mutex<HashMap<u32, mpsc::Sender<proto::Envelope>>>>,
}

impl Peer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            connections: Default::default(),
            next_connection_id: Default::default(),
        })
    }

    pub async fn add_connection(
        self: &Arc<Self>,
        conn: Conn,
    ) -> (
        ConnectionId,
        impl Future<Output = anyhow::Result<()>> + Send,
        mpsc::Receiver<Box<dyn AnyTypedEnvelope>>,
    ) {
        let connection_id = ConnectionId(
            self.next_connection_id
                .fetch_add(1, atomic::Ordering::SeqCst),
        );
        let (mut incoming_tx, incoming_rx) = mpsc::channel(64);
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(64);
        let connection = Connection {
            outgoing_tx,
            next_message_id: Default::default(),
            response_channels: Default::default(),
        };
        let mut writer = MessageStream::new(conn.tx);
        let mut reader = MessageStream::new(conn.rx);

        let this = self.clone();
        let response_channels = connection.response_channels.clone();
        let handle_io = async move {
            loop {
                let read_message = reader.read_message().fuse();
                futures::pin_mut!(read_message);
                loop {
                    futures::select_biased! {
                        incoming = read_message => match incoming {
                            Ok(incoming) => {
                                if let Some(responding_to) = incoming.responding_to {
                                    let channel = response_channels.lock().await.remove(&responding_to);
                                    if let Some(mut tx) = channel {
                                        tx.send(incoming).await.ok();
                                    } else {
                                        log::warn!("received RPC response to unknown request {}", responding_to);
                                    }
                                } else {
                                    if let Some(envelope) = proto::build_typed_envelope(connection_id, incoming) {
                                        if incoming_tx.send(envelope).await.is_err() {
                                            response_channels.lock().await.clear();
                                            this.connections.write().await.remove(&connection_id);
                                            return Ok(())
                                        }
                                    } else {
                                        log::error!("unable to construct a typed envelope");
                                    }
                                }

                                break;
                            }
                            Err(error) => {
                                response_channels.lock().await.clear();
                                this.connections.write().await.remove(&connection_id);
                                Err(error).context("received invalid RPC message")?;
                            }
                        },
                        outgoing = outgoing_rx.recv().fuse() => match outgoing {
                            Some(outgoing) => {
                                if let Err(result) = writer.write_message(&outgoing).await {
                                    response_channels.lock().await.clear();
                                    this.connections.write().await.remove(&connection_id);
                                    Err(result).context("failed to write RPC message")?;
                                }
                            }
                            None => {
                                response_channels.lock().await.clear();
                                this.connections.write().await.remove(&connection_id);
                                return Ok(())
                            }
                        }
                    }
                }
            }
        };

        self.connections
            .write()
            .await
            .insert(connection_id, connection);

        (connection_id, handle_io, incoming_rx)
    }

    pub async fn disconnect(&self, connection_id: ConnectionId) {
        self.connections.write().await.remove(&connection_id);
    }

    pub async fn reset(&self) {
        self.connections.write().await.clear();
    }

    pub fn request<T: RequestMessage>(
        self: &Arc<Self>,
        receiver_id: ConnectionId,
        request: T,
    ) -> impl Future<Output = Result<T::Response>> {
        self.request_internal(None, receiver_id, request)
    }

    pub fn forward_request<T: RequestMessage>(
        self: &Arc<Self>,
        sender_id: ConnectionId,
        receiver_id: ConnectionId,
        request: T,
    ) -> impl Future<Output = Result<T::Response>> {
        self.request_internal(Some(sender_id), receiver_id, request)
    }

    pub fn request_internal<T: RequestMessage>(
        self: &Arc<Self>,
        original_sender_id: Option<ConnectionId>,
        receiver_id: ConnectionId,
        request: T,
    ) -> impl Future<Output = Result<T::Response>> {
        let this = self.clone();
        let (tx, mut rx) = mpsc::channel(1);
        async move {
            let mut connection = this.connection(receiver_id).await?;
            let message_id = connection
                .next_message_id
                .fetch_add(1, atomic::Ordering::SeqCst);
            connection
                .response_channels
                .lock()
                .await
                .insert(message_id, tx);
            connection
                .outgoing_tx
                .send(request.into_envelope(message_id, None, original_sender_id.map(|id| id.0)))
                .await
                .map_err(|_| anyhow!("connection was closed"))?;
            let response = rx
                .recv()
                .await
                .ok_or_else(|| anyhow!("connection was closed"))?;
            if let Some(proto::envelope::Payload::Error(error)) = &response.payload {
                Err(anyhow!("request failed").context(error.message.clone()))
            } else {
                T::Response::from_envelope(response)
                    .ok_or_else(|| anyhow!("received response of the wrong type"))
            }
        }
    }

    pub fn send<T: EnvelopedMessage>(
        self: &Arc<Self>,
        receiver_id: ConnectionId,
        message: T,
    ) -> impl Future<Output = Result<()>> {
        let this = self.clone();
        async move {
            let mut connection = this.connection(receiver_id).await?;
            let message_id = connection
                .next_message_id
                .fetch_add(1, atomic::Ordering::SeqCst);
            connection
                .outgoing_tx
                .send(message.into_envelope(message_id, None, None))
                .await?;
            Ok(())
        }
    }

    pub fn forward_send<T: EnvelopedMessage>(
        self: &Arc<Self>,
        sender_id: ConnectionId,
        receiver_id: ConnectionId,
        message: T,
    ) -> impl Future<Output = Result<()>> {
        let this = self.clone();
        async move {
            let mut connection = this.connection(receiver_id).await?;
            let message_id = connection
                .next_message_id
                .fetch_add(1, atomic::Ordering::SeqCst);
            connection
                .outgoing_tx
                .send(message.into_envelope(message_id, None, Some(sender_id.0)))
                .await?;
            Ok(())
        }
    }

    pub fn respond<T: RequestMessage>(
        self: &Arc<Self>,
        receipt: Receipt<T>,
        response: T::Response,
    ) -> impl Future<Output = Result<()>> {
        let this = self.clone();
        async move {
            let mut connection = this.connection(receipt.sender_id).await?;
            let message_id = connection
                .next_message_id
                .fetch_add(1, atomic::Ordering::SeqCst);
            connection
                .outgoing_tx
                .send(response.into_envelope(message_id, Some(receipt.message_id), None))
                .await?;
            Ok(())
        }
    }

    pub fn respond_with_error<T: RequestMessage>(
        self: &Arc<Self>,
        receipt: Receipt<T>,
        response: proto::Error,
    ) -> impl Future<Output = Result<()>> {
        let this = self.clone();
        async move {
            let mut connection = this.connection(receipt.sender_id).await?;
            let message_id = connection
                .next_message_id
                .fetch_add(1, atomic::Ordering::SeqCst);
            connection
                .outgoing_tx
                .send(response.into_envelope(message_id, Some(receipt.message_id), None))
                .await?;
            Ok(())
        }
    }

    fn connection(
        self: &Arc<Self>,
        connection_id: ConnectionId,
    ) -> impl Future<Output = Result<Connection>> {
        let this = self.clone();
        async move {
            let connections = this.connections.read().await;
            let connection = connections
                .get(&connection_id)
                .ok_or_else(|| anyhow!("no such connection: {}", connection_id))?;
            Ok(connection.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TypedEnvelope;
    use async_tungstenite::tungstenite::Message as WebSocketMessage;
    use futures::StreamExt as _;

    #[test]
    fn test_request_response() {
        smol::block_on(async move {
            // create 2 clients connected to 1 server
            let server = Peer::new();
            let client1 = Peer::new();
            let client2 = Peer::new();

            let (client1_to_server_conn, server_to_client_1_conn, _) = Conn::in_memory();
            let (client1_conn_id, io_task1, _) =
                client1.add_connection(client1_to_server_conn).await;
            let (_, io_task2, incoming1) = server.add_connection(server_to_client_1_conn).await;

            let (client2_to_server_conn, server_to_client_2_conn, _) = Conn::in_memory();
            let (client2_conn_id, io_task3, _) =
                client2.add_connection(client2_to_server_conn).await;
            let (_, io_task4, incoming2) = server.add_connection(server_to_client_2_conn).await;

            smol::spawn(io_task1).detach();
            smol::spawn(io_task2).detach();
            smol::spawn(io_task3).detach();
            smol::spawn(io_task4).detach();
            smol::spawn(handle_messages(incoming1, server.clone())).detach();
            smol::spawn(handle_messages(incoming2, server.clone())).detach();

            assert_eq!(
                client1
                    .request(client1_conn_id, proto::Ping { id: 1 },)
                    .await
                    .unwrap(),
                proto::Pong { id: 1 }
            );

            assert_eq!(
                client2
                    .request(client2_conn_id, proto::Ping { id: 2 },)
                    .await
                    .unwrap(),
                proto::Pong { id: 2 }
            );

            assert_eq!(
                client1
                    .request(
                        client1_conn_id,
                        proto::OpenBuffer {
                            worktree_id: 1,
                            path: "path/one".to_string(),
                        },
                    )
                    .await
                    .unwrap(),
                proto::OpenBufferResponse {
                    buffer: Some(proto::Buffer {
                        id: 101,
                        content: "path/one content".to_string(),
                        history: vec![],
                        selections: vec![],
                    }),
                }
            );

            assert_eq!(
                client2
                    .request(
                        client2_conn_id,
                        proto::OpenBuffer {
                            worktree_id: 2,
                            path: "path/two".to_string(),
                        },
                    )
                    .await
                    .unwrap(),
                proto::OpenBufferResponse {
                    buffer: Some(proto::Buffer {
                        id: 102,
                        content: "path/two content".to_string(),
                        history: vec![],
                        selections: vec![],
                    }),
                }
            );

            client1.disconnect(client1_conn_id).await;
            client2.disconnect(client1_conn_id).await;

            async fn handle_messages(
                mut messages: mpsc::Receiver<Box<dyn AnyTypedEnvelope>>,
                peer: Arc<Peer>,
            ) -> Result<()> {
                while let Some(envelope) = messages.next().await {
                    let envelope = envelope.into_any();
                    if let Some(envelope) = envelope.downcast_ref::<TypedEnvelope<proto::Ping>>() {
                        let receipt = envelope.receipt();
                        peer.respond(
                            receipt,
                            proto::Pong {
                                id: envelope.payload.id,
                            },
                        )
                        .await?
                    } else if let Some(envelope) =
                        envelope.downcast_ref::<TypedEnvelope<proto::OpenBuffer>>()
                    {
                        let message = &envelope.payload;
                        let receipt = envelope.receipt();
                        let response = match message.path.as_str() {
                            "path/one" => {
                                assert_eq!(message.worktree_id, 1);
                                proto::OpenBufferResponse {
                                    buffer: Some(proto::Buffer {
                                        id: 101,
                                        content: "path/one content".to_string(),
                                        history: vec![],
                                        selections: vec![],
                                    }),
                                }
                            }
                            "path/two" => {
                                assert_eq!(message.worktree_id, 2);
                                proto::OpenBufferResponse {
                                    buffer: Some(proto::Buffer {
                                        id: 102,
                                        content: "path/two content".to_string(),
                                        history: vec![],
                                        selections: vec![],
                                    }),
                                }
                            }
                            _ => {
                                panic!("unexpected path {}", message.path);
                            }
                        };

                        peer.respond(receipt, response).await?
                    } else {
                        panic!("unknown message type");
                    }
                }

                Ok(())
            }
        });
    }

    #[test]
    fn test_disconnect() {
        smol::block_on(async move {
            let (client_conn, mut server_conn, _) = Conn::in_memory();

            let client = Peer::new();
            let (connection_id, io_handler, mut incoming) =
                client.add_connection(client_conn).await;

            let (mut io_ended_tx, mut io_ended_rx) = postage::barrier::channel();
            smol::spawn(async move {
                io_handler.await.ok();
                io_ended_tx.send(()).await.unwrap();
            })
            .detach();

            let (mut messages_ended_tx, mut messages_ended_rx) = postage::barrier::channel();
            smol::spawn(async move {
                incoming.next().await;
                messages_ended_tx.send(()).await.unwrap();
            })
            .detach();

            client.disconnect(connection_id).await;

            io_ended_rx.recv().await;
            messages_ended_rx.recv().await;
            assert!(server_conn
                .send(WebSocketMessage::Binary(vec![]))
                .await
                .is_err());
        });
    }

    #[test]
    fn test_io_error() {
        smol::block_on(async move {
            let (client_conn, server_conn, _) = Conn::in_memory();
            drop(server_conn);

            let client = Peer::new();
            let (connection_id, io_handler, mut incoming) =
                client.add_connection(client_conn).await;
            smol::spawn(io_handler).detach();
            smol::spawn(async move { incoming.next().await }).detach();

            let err = client
                .request(connection_id, proto::Ping { id: 42 })
                .await
                .unwrap_err();
            assert_eq!(err.to_string(), "connection was closed");
        });
    }
}
