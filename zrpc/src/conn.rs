use async_tungstenite::tungstenite::{Error as WebSocketError, Message as WebSocketMessage};
use futures::{SinkExt as _, StreamExt as _};

pub struct Conn {
    pub(crate) tx:
        Box<dyn 'static + Send + Unpin + futures::Sink<WebSocketMessage, Error = WebSocketError>>,
    pub(crate) rx: Box<
        dyn 'static
            + Send
            + Unpin
            + futures::Stream<Item = Result<WebSocketMessage, WebSocketError>>,
    >,
}

impl Conn {
    pub fn new<S>(stream: S) -> Self
    where
        S: 'static
            + Send
            + Unpin
            + futures::Sink<WebSocketMessage, Error = WebSocketError>
            + futures::Stream<Item = Result<WebSocketMessage, WebSocketError>>,
    {
        let (tx, rx) = stream.split();
        Self {
            tx: Box::new(tx),
            rx: Box::new(rx),
        }
    }

    pub async fn send(&mut self, message: WebSocketMessage) -> Result<(), WebSocketError> {
        self.tx.send(message).await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn in_memory() -> (Self, Self, postage::watch::Sender<Option<()>>) {
        let (kill_tx, mut kill_rx) = postage::watch::channel_with(None);
        postage::stream::Stream::try_recv(&mut kill_rx).unwrap();

        let (a_tx, a_rx) = Self::channel(kill_rx.clone());
        let (b_tx, b_rx) = Self::channel(kill_rx);
        (
            Self { tx: a_tx, rx: b_rx },
            Self { tx: b_tx, rx: a_rx },
            kill_tx,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    fn channel(
        kill_rx: postage::watch::Receiver<Option<()>>,
    ) -> (
        Box<dyn Send + Unpin + futures::Sink<WebSocketMessage, Error = WebSocketError>>,
        Box<dyn Send + Unpin + futures::Stream<Item = Result<WebSocketMessage, WebSocketError>>>,
    ) {
        use futures::{future, stream, SinkExt as _, StreamExt as _};
        use std::io::{Error, ErrorKind};

        let (tx, rx) = futures::channel::mpsc::unbounded::<WebSocketMessage>();
        let tx = tx
            .sink_map_err(|e| WebSocketError::from(Error::new(ErrorKind::Other, e)))
            .with({
                let kill_rx = kill_rx.clone();
                move |msg| {
                    if kill_rx.borrow().is_none() {
                        future::ready(Ok(msg))
                    } else {
                        future::ready(Err(Error::new(ErrorKind::Other, "connection killed").into()))
                    }
                }
            });
        let rx = stream::select(
            rx.map(Ok),
            kill_rx.filter_map(|kill| {
                if let Some(_) = kill {
                    future::ready(Some(Err(
                        Error::new(ErrorKind::Other, "connection killed").into()
                    )))
                } else {
                    future::ready(None)
                }
            }),
        );

        (Box::new(tx), Box::new(rx))
    }
}
