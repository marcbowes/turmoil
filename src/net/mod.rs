//! This module contains the simulated TCP/UDP networking types.
//!
//! They mirror [tokio::net](https://docs.rs/tokio/latest/tokio/net/) to provide
//! a high fidelity implementation.

use std::net::SocketAddr;

use crate::Message;

use bytes::Bytes;
use tokio::sync::oneshot;

mod listener;
pub use listener::TcpListener;

mod stream;
pub use stream::TcpStream;

mod udp;
pub use udp::{recv, recv_from, send};

/// (Local, Remote)
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub(crate) struct SocketPair(pub(crate) SocketAddr, pub(crate) SocketAddr);

impl SocketPair {
    pub(crate) fn new(local: SocketAddr, remote: SocketAddr) -> Self {
        assert_ne!(local, remote);
        Self(local, remote)
    }
}

/// TCP message types.
///
/// This is a parody of the encapsulation TCP does. We don't implement all of
/// TCP (padding, checksums, sequencing), nor are we faithful for the bits we do
/// implement. The point is to simulate real world interactions, and so we offer
/// just enough control over delivery, without getting too complicated.
#[derive(Debug)]
pub(crate) enum Segment {
    /// Message used to initiate a new connection with a host.
    Syn(Syn),

    ///
    Data(Bytes),
}

#[derive(Debug)]
pub(crate) struct Syn {
    /// FIXME: Links only deal in Ips, so we need to smuggle the server socket
    /// through to route correctly once the host receives the message.
    pub(crate) dst: SocketAddr,

    /// Notify the peer that the connection has been accepted.
    ///
    /// To connect, we only send one message (the SYN) and the rest (SYN-ACK and
    /// ACK) is instantaneous. The SYN message is subject to turmoil when a host
    /// connects, such as added delay, which sufficiently simulates the 3-step
    /// handshake.
    pub(crate) notify: oneshot::Sender<()>,
}

impl Message for Segment {
    fn write_json(&self, dst: &mut dyn std::io::Write) {
        match self {
            Segment::Syn(_) => write!(dst, "Syn").unwrap(),
            Segment::Data(_) => write!(dst, "Data").unwrap(),
        }
    }
}
