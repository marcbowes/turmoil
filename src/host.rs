use crate::envelope::{self, Envelope};
use crate::message::{self, downcast};
use crate::net::{Segment, Syn, TcpStream};
use crate::top::Pair;
use crate::{version, Message};

use indexmap::IndexMap;
use std::any::TypeId;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use tokio::sync::Notify;
use tokio::time::{Duration, Instant};

/// A host in the simulated network.
///
/// Hosts support two networking modes:
/// - Datagram
/// - Stream
///
/// Both modes may be used by host software simultaneously.
pub(crate) struct Host {
    /// Host address
    pub(crate) addr: SocketAddr,

    /// Messages in-flight to the host. Some of these may still be "on the
    /// network".
    inbox: IndexMap<SocketAddr, VecDeque<Envelope>>,

    /// Signaled when a message becomes available to receive.
    pub(crate) notify: Rc<Notify>,

    /// Tcp interface for the host.
    pub(crate) tcp: Tcp,

    /// Current instant at the host.
    pub(crate) now: Instant,

    epoch: Instant,

    /// Current host version. This is incremented each time a network operation
    /// occurs.
    pub(crate) version: u64,
}

impl Host {
    pub(crate) fn new(addr: SocketAddr, now: Instant, notify: Rc<Notify>) -> Host {
        Host {
            addr,
            inbox: IndexMap::new(),
            notify,
            tcp: Tcp::new(),
            now,
            epoch: now,
            version: 0,
        }
    }

    /// Returns how long the host has been executing for in virtual time
    pub(crate) fn elapsed(&self) -> Duration {
        self.now - self.epoch
    }

    /// Bump the version for this host and return a dot.
    ///
    /// Called when a host establishes a new connection with a remote peer.
    pub(crate) fn bump(&mut self) -> version::Dot {
        self.bump_version();
        self.dot()
    }

    fn bump_version(&mut self) {
        self.version += 1;
    }

    /// Returns a dot for the host at its current version
    pub(crate) fn dot(&self) -> version::Dot {
        version::Dot {
            host: self.addr,
            version: self.version,
        }
    }

    pub(crate) fn receive_from_network(&mut self, envelope: Envelope) {
        self.inbox
            .entry(envelope.src.host)
            .or_default()
            .push_back(envelope);

        // Establised connections wake up the stream.
        // let notify = if message.type_id() == TypeId::of::<StreamEnvelope>() {
        //     let stream = downcast::<StreamEnvelope>(message);
        //     let inbox = self
        //         .connections
        //         .get_mut(&stream.pair)
        //         .expect("no connection");
        //     inbox.notify
        // } else {
        //     self.notify
        // };

        self.notify.notify_one();
    }

    // FIXME: since multiple messages could arrive at the same instant, this
    // code is currently broken (since the notification is clear, but there
    // maybe be other messages ready to go). See other methods too, like
    // recv_from.
    //
    // This implementation does not respect message delivery order. If host A
    // and host B are ordered (by addr), and B sends before A, then this method
    // will return A's message before B's.
    pub(crate) fn recv(&mut self) -> (Option<Envelope>, Rc<Notify>) {
        let notify = self.notify.clone();

        for deque in self.inbox.values_mut() {
            if let Some(envelope) = deque.pop_front() {
                self.bump_version();
                return (Some(envelope), notify);
            }
        }

        (None, notify)
    }

    // FIXME: See recv
    pub(crate) fn recv_from(&mut self, peer: SocketAddr) -> (Option<Envelope>, Rc<Notify>) {
        let deque = self.inbox.entry(peer).or_default();
        let notify = self.notify.clone();

        match deque.pop_front() {
            Some(envelope) => {
                self.bump_version();
                (Some(envelope), notify)
            }
            _ => (None, notify),
        }
    }

    // pub(crate) fn recv_on(&mut self, pair: SocketPair) -> Option<Segment> {
    //     let now = Instant::now();

    //     let deque = self
    //         .connections
    //         .get_mut(&pair)
    //         .map(|i| &mut i.deque)
    //         .expect("no connection");

    //     match deque.front() {
    //         Some(StreamEnvelope {
    //             instructions: DeliveryInstructions::DeliverAt(time),
    //             ..
    //         }) if *time <= now => {
    //             let ret = deque.pop_front().map(|e| e.segment);
    //             self.bump_version();
    //             ret
    //         }
    //         _ => None,
    //     }
    // }

    pub(crate) fn tick(&mut self, now: Instant) {
        self.now = now;
    }
}

/// Host software that implements the simulated Transmission Control Protocol
/// (TCP).
pub(crate) struct Tcp {
    /// Bound server sockets
    binds: IndexMap<SocketAddr, Inbox<Envelope>>,

    /// Active connections, keyed by (bind, remote)
    connections: IndexMap<Pair, Inbox<Segment>>,
}

/// A simple unbounded channel.
struct Inbox<T> {
    /// Queued items
    deque: VecDeque<T>,

    /// Signaled when an item is available to recv
    notify: Rc<Notify>,
}

impl Tcp {
    fn new() -> Self {
        Self {
            binds: IndexMap::new(),
            connections: IndexMap::new(),
        }
    }

    pub(crate) fn bind(&mut self, addr: SocketAddr) -> io::Result<Rc<Notify>> {
        use indexmap::map::Entry::Vacant;

        let notify = match self.binds.entry(addr) {
            Vacant(e) => e
                .insert(Inbox {
                    deque: VecDeque::new(),
                    notify: Rc::new(Notify::new()),
                })
                .notify
                .clone(),
            _ => return Err(io::Error::new(io::ErrorKind::AddrInUse, addr.to_string())),
        };

        Ok(notify)
    }

    pub(crate) fn connect(&mut self) {}

    pub(crate) fn accept(&mut self, addr: SocketAddr) -> Option<(TcpStream, SocketAddr)> {
        if let Some(Envelope { src, message }) = self.binds[&addr].deque.pop_front() {
            let seg = message::downcast::<Segment>(message);

            let syn = match seg {
                Segment::Syn(syn) => syn,
                _ => panic!("invalid protocol {:?}", seg),
            };

            // Perform the SYN-ACK, returning early if the peer has hung up to
            // avoid mutations.
            syn.notify.send(()).ok()?;

            let pair = Pair(addr, src.host);
            let inbox = Inbox {
                deque: VecDeque::new(),
                notify: Rc::new(Notify::new()),
            };
            let notify = inbox.notify.clone();

            assert!(
                self.connections.insert(pair, inbox).is_none(),
                "pair is already connect {:?}",
                pair
            );

            return Some((TcpStream::new(pair, notify), src.host));
        }

        None
    }

    pub(crate) fn unbind(&mut self, addr: SocketAddr) {}
}
