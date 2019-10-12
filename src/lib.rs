use std::thread;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::net::{UdpSocket, SocketAddr};

use chashmap::CHashMap;
use crossbeam::channel;

mod config;
use config::Config;

mod event;
pub use event::Event;

mod datagram;
use datagram::Datagram;

mod packet;
pub use packet::Packet;

mod connection;
use connection::Connection;

#[derive(Debug, Clone)]
pub struct Socket {
    sender: channel::Sender<Packet>,
    receiver: channel::Receiver<Event>
}

impl Socket {
    pub fn bind_any(config: Config) -> Self {
        Self::bind("0.0.0.0:0".parse().unwrap(), config)
    }

    pub fn bind(address: SocketAddr, config: Config) -> Self {
        let connections: Arc<CHashMap<SocketAddr, Connection>> = Arc::new(CHashMap::new());

        let (outbound_sender, outbound_receiver) = channel::unbounded::<Packet>();
        let (inbound_sender, inbound_receiver) = channel::bounded::<Event>(config.event_capacity);

        let socket = UdpSocket::bind(address).expect("Unable to bind UDP-socket.");

        {
            let socket = socket.try_clone().expect("Unable to clone UDP-socket.");
            let inbound_sender = inbound_sender.clone();
            let connections = connections.clone();
            thread::spawn(move || {
                loop {

                    // Receive datagrams:
                    let mut buffer = [0; 1450];
                    match socket.recv_from(&mut buffer) {
                        Ok((bytes_read, address)) => {
                            match bincode::deserialize::<Datagram>(&buffer[..bytes_read]) {
                                Ok(Datagram { payload, rtt_seq, rtt_ack }) => {
                                    connections.alter(address.clone(), |conn| {
                                        let mut connection = match conn {
                                            Some(mut connection) => {
                                                connection.last_interaction = Instant::now();

                                                connection
                                            },
                                            None => {
                                                let connection = Connection::new();
                                                inbound_sender.send(Event::Connected(address)).expect("Unable to dispatch event to channel.");

                                                connection
                                            }
                                        };

                                        println!("RTT seq: {}, ack: {}", rtt_seq, rtt_ack);

                                        if let Some(instant) = connection.rtt_timers.remove(&rtt_ack) {
                                            let rtt_sample = instant.elapsed();

                                            match connection.rtt {
                                                Some(rtt) => {
                                                    connection.rtt = Some((rtt.mul_f32(1.0 - config.rtt_alpha)) + rtt_sample.mul_f32(config.rtt_alpha));
                                                },
                                                None => {
                                                    connection.rtt = Some(rtt_sample);
                                                }
                                            }

                                            println!("Estimated RTT: {} ms", connection.rtt.unwrap().as_millis());
                                        }

                                        connection.rtt_seq_remote = rtt_seq;
                                        inbound_sender.send(Event::Received(address, payload)).expect("Unable to dispatch event to channel.");
                                        
                                        Some(connection)
                                    });
                                },
                                Err(msg) => println!("Error parsing payload: {}", msg)
                            }
                        },
                        Err(msg) => {
                            panic!("Encountered IO error: {}", msg);
                        }
                    }
                }
            });
        }

        // Sender thread:
        {
            let socket = socket.try_clone().expect("Unable to clone UDP-socket.");
            let inbound_sender = inbound_sender.clone();
            let connections = connections.clone();
            thread::spawn(move || {
                loop {
                    match outbound_receiver.recv() {
                        Ok(Packet { address, payload }) => {

                            connections.alter(address.clone(), |conn| {
                                let mut connection = match conn {
                                    Some(connection) => connection,
                                    None => {
                                        let connection = Connection::new();
                                        inbound_sender.send(Event::Connected(address)).expect("Unable to dispatch event to channel.");

                                        connection
                                    }
                                };

                                connection.rtt_seq_local = connection.rtt_seq_local.wrapping_add(1);
                                connection.rtt_timers.insert(connection.rtt_seq_local, Instant::now());

                                // Trim queue:
                                while connection.rtt_timers.len() > config.rtt_queue_capacity {
                                    connection.rtt_timers.pop_front();
                                }

                                let buffer = bincode::serialize(&Datagram::new(payload, connection.rtt_seq_local, connection.rtt_seq_remote)).expect("Unable to serialize datagram.");
                                match socket.send_to(&buffer[0..], address) {
                                    Ok(_) => {},
                                    Err(msg) => println!("Error sending packet: {}", msg)
                                }
                                
                                Some(connection)
                            });
                        },
                        Err(_) => {
                            break; // Is empty and disconnected, terminate thread.
                        }
                    }
                }
            });
        }

        // Timeout checker thread:
        {
            let connections = connections.clone();
            let inbound_sender = inbound_sender.clone();
            thread::spawn(move || {
                loop {
                    {
                        connections.retain(|address, connection: &Connection| {
                            if connection.last_interaction.elapsed() >= config.timeout {
                                inbound_sender.try_send(Event::Disconnected(address.clone())).expect("Unable to dispatch event to channel.");
                                false
                            } else {
                                true
                            }
                        });
                    }

                    thread::sleep(config.timeout);
                }
            });
        }
        
        Self {
            sender: outbound_sender,
            receiver: inbound_receiver
        }
    }

    pub fn event_receiver(&self) -> channel::Receiver<Event> {
        self.receiver.clone()
    }

    pub fn packet_sender(&self) -> channel::Sender<Packet> {
        self.sender.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{Socket, Event, SocketAddr, Config, Packet};
    
    #[test]
    fn sending_and_receiving() {

        let server_address: SocketAddr = "127.0.0.1:38000".parse().unwrap();
        let client_address: SocketAddr = "127.0.0.1:38001".parse().unwrap();

        let server = Socket::bind(server_address, Config::default());
        let client = Socket::bind(client_address, Config::default());

        let j1 = std::thread::spawn(move || {
            for i in 0..10 {
                server.packet_sender().send(Packet::new(client_address, "Hello, Client!".as_bytes().to_vec()));
                std::thread::sleep_ms(50);
            }
            loop {
                match server.event_receiver().recv() {
                    Ok(Event::Connected(addr)) => {
                        println!("Client connected to server!");
                        assert_eq!(addr, client_address);
                    },
                    Ok(Event::Received(addr, payload)) => {
                        println!("Server received a packet from the client! Content: {}", std::str::from_utf8(&payload).unwrap());
                        assert_eq!(addr, client_address);
                        assert_eq!("Hello, Server!".as_bytes().to_vec(), payload);
                    },
                    Ok(Event::Disconnected(addr)) => {
                        println!("Client disconnnected from server!");
                        assert_eq!(addr, client_address);
                        break;
                    },
                    Err(err) => {
                        panic!("Error: {}", err);
                    }
                }
            }
        });
        
        let j2 = std::thread::spawn(move || {
            for i in 0..10 {
                client.packet_sender().send(Packet::new(server_address, "Hello, Server!".as_bytes().to_vec()));
                std::thread::sleep_ms(50);
            }
            loop {
                match client.event_receiver().recv() {
                    Ok(Event::Connected(addr)) => {
                        println!("Server connected to client!");
                        assert_eq!(addr, server_address);
                    },
                    Ok(Event::Received(addr, payload)) => {
                        println!("Client received a packet from the server! Content: {}", std::str::from_utf8(&payload).unwrap());
                        assert_eq!(addr, server_address);
                        assert_eq!("Hello, Client!".as_bytes().to_vec(), payload);
                    },
                    Ok(Event::Disconnected(addr)) => {
                        println!("Server disconnnected from client!");
                        assert_eq!(addr, server_address);
                        break;
                    },
                    Err(err) => {
                        panic!("Error: {}", err);
                    }
                }
            }
        });

        j1.join().unwrap();
        j2.join().unwrap();
    }
}