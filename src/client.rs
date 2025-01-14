use futures::StreamExt;
use std::future::Future;
use thiserror::Error;
use tokio::{
    io::split,
    net::{TcpStream, ToSocketAddrs, UdpSocket},
};

use crate::{
    connection::ConnectionError, receiver, sender, Config, Connection, Delivery, Receiver, Sender,
};

#[cfg(feature = "rustls")]
use tokio_rustls::{rustls::ClientConfig, webpki::DNSName, TlsConnector};

#[cfg(feature = "rustls")]
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum ClientEvent {
    Connected,
    Received(Vec<u8>),
    Disconnected,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("Unable to create client.")]
    Io(#[from] std::io::Error),
    #[error("Unable to establish connection.")]
    Connection(#[from] ConnectionError),
    #[error("Unable to dispatch event.")]
    Event(#[from] receiver::TrySendError<ClientEvent>),
}

pub type ClientSender = Sender<(Vec<u8>, Delivery)>;
pub type ClientReceiver = Receiver<ClientEvent>;

pub struct Client;

impl Client {
    /// Connect to a server.
    /// Returns a [`Sender`], [`Receiver`] and a [`Future`] which must be awaited in an async executor (see the examples in the [repository](https://github.com/oskarbraten/zelda/)).
    /// The client can run in a separate thread and messages/events can be sent/received in a synchronous context.
    pub fn connect<A: ToSocketAddrs>(
        address: A,
        config: Config,
        #[cfg(feature = "rustls")] domain: DNSName,
        #[cfg(feature = "rustls")] client_config: ClientConfig,
        token: Vec<u8>,
    ) -> (
        Sender<(Vec<u8>, Delivery)>,
        Receiver<ClientEvent>,
        impl Future<Output = Result<(), ClientError>>,
    ) {
        let (outbound_sender, outbound_receiver) = sender::channel::<(Vec<u8>, Delivery)>();
        let (inbound_sender, inbound_receiver) =
            receiver::channel::<ClientEvent>(config.event_capacity);

        let task = Self::task(
            address,
            config,
            #[cfg(feature = "rustls")]
            domain,
            #[cfg(feature = "rustls")]
            client_config,
            token,
            inbound_sender,
            outbound_receiver,
        );

        (
            Sender::new(outbound_sender),
            Receiver::new(inbound_receiver),
            task,
        )
    }

    async fn task<A: ToSocketAddrs>(
        address: A,
        config: Config,
        #[cfg(feature = "rustls")] domain: DNSName,
        #[cfg(feature = "rustls")] client_config: ClientConfig,
        token: Vec<u8>,
        mut inbound_sender: receiver::InnerSender<ClientEvent>,
        mut outbound_receiver: sender::InnerReceiver<(Vec<u8>, Delivery)>,
    ) -> Result<(), ClientError> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect(&address).await?;

        let stream = TcpStream::connect(&address).await?;
        stream.set_nodelay(true).unwrap();

        #[cfg(not(feature = "rustls"))]
        let (mut read_stream, write_stream) = split(stream);

        #[cfg(feature = "rustls")]
        let (mut read_stream, write_stream) = {
            let connector = TlsConnector::from(Arc::new(client_config));
            let stream = connector.connect(domain.as_ref(), stream).await?;
            split(stream)
        };

        let (id, connection) =
            Connection::connect(&socket, &mut read_stream, write_stream, token).await?;
        inbound_sender.try_send(ClientEvent::Connected)?;

        let mut recv_buffer = [0u8; std::u16::MAX as usize];
        loop {
            tokio::select! {
                result = Connection::read(&mut read_stream, config.max_reliable_size) => {
                    match result {
                        Ok(data) => {
                            inbound_sender.try_send(ClientEvent::Received(data))?;
                        },
                        Err(err) => {
                            log::debug!("Error reading frame (TCP): {:#?}", err);
                            inbound_sender.try_send(ClientEvent::Disconnected)?;
                            return Err(err.into());
                        }
                    }
                },
                result = socket.recv(&mut recv_buffer) => {
                    if let Ok(bytes_read) = result {
                        // Must receive more than tag (u64) bytes
                        if bytes_read > 8 {
                            let tag = &recv_buffer[0..8];
                            let data = &recv_buffer[8..bytes_read];

                            if connection.verify(data, tag) {
                                inbound_sender.try_send(ClientEvent::Received(data.to_vec()))?;
                            }
                        }
                    }
                },
                result = outbound_receiver.next() => {
                    if let Some((mut data, delivery)) = result {
                        match delivery {
                            Delivery::Reliable => match connection.write(&data).await {
                                Ok(()) => {},
                                Err(err) => log::debug!("Error writing message (TCP): {}", err)
                            },
                            Delivery::Unreliable => {
                                let mut bytes = connection.sign(&data).to_vec(); // Add tag.
                                bytes.extend(&id.to_be_bytes()); // Add id.
                                bytes.append(&mut data); // Add data.

                                match socket.send(&bytes).await {
                                    Ok(_) => {},
                                    Err(err) => log::debug!("Error writing message (UDP): {}", err)
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
