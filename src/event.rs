use std::time::Duration;
use std::net::SocketAddr;
use super::datagram::Payload;

pub enum Event {
    Connected(SocketAddr),
    /// Received a payload on the specified connection.
    /// The last tuple parameter is the estimated RTT so far if it has been calculated.
    Received {
        address: SocketAddr,
        payload: Payload,
        rtt: Option<Duration>,
        rtt_offset: Option<Duration>
    },
    Disconnected(SocketAddr)
}