use crate::envelope::{self, Envelope};
use crate::message::{self, downcast};
use crate::net::{Segment, SocketPair, Syn, TcpStream};
use crate::top::Pair;
use crate::{version, Message};

use indexmap::IndexMap;
use std::any::TypeId;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use tokio::sync::{Notify, Semaphore};
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

    // TODO: Move Udp to it's own thing.
    /// Messages in-flight to the host. Some of these may still be "on the
    /// network".
    udp_inbox: IndexMap<SocketAddr, VecDeque<Envelope>>,

    /// Udp messages are available to read.
    pub(crate) udp_notify: Rc<Semaphore>,

    /// Tcp interface for the host.
    pub(crate) tcp: Tcp,

    /// Ports 1024 - 65535 for client connections.
    next_ephemeral_port: u16,

    /// Current instant at the host.
    pub(crate) now: Instant,

    epoch: Instant,

    /// Current host version. This is incremented each time a network operation
    /// occurs.
    pub(crate) version: u64,
}

impl Host {
    pub(crate) fn new(addr: SocketAddr, now: Instant, udp_notify: Rc<Semaphore>) -> Host {
        Host {
            addr,
            udp_inbox: IndexMap::new(),
            udp_notify,
            tcp: Tcp::new(),
            next_ephemeral_port: 1024,
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

    // We don't recycle yet... should be sufficient for now.
    pub(crate) fn assign_ephemeral_port(&mut self) -> u16 {
        assert_ne!(65535, self.next_ephemeral_port, "ephemeral ports exhausted");
        if self.next_ephemeral_port == self.addr.port() {
            self.next_ephemeral_port += 1;
        }

        let ret = self.next_ephemeral_port;
        self.next_ephemeral_port += 1;
        ret
    }

    pub(crate) fn receive_from_network(&mut self, envelope: Envelope) {
        // Deliver to the appropriate inbox and wake up the receiver
        let notify = if envelope.message.as_ref().type_id() == TypeId::of::<Segment>() {
            self.tcp.receive_from_network(envelope)
        } else {
            self.udp_inbox
                .entry(envelope.src.host)
                .or_default()
                .push_back(envelope);

            &self.udp_notify
        };

        notify.add_permits(1);
    }

    // FIXME: since multiple messages could arrive at the same instant, this
    // code is currently broken (since the notification is clear, but there
    // maybe be other messages ready to go). See other methods too, like
    // recv_from.
    //
    // This implementation does not respect message delivery order. If host A
    // and host B are ordered (by addr), and B sends before A, then this method
    // will return A's message before B's.
    pub(crate) fn recv(&mut self) -> (Option<Envelope>, Rc<Semaphore>) {
        let notify = self.udp_notify.clone();

        for deque in self.udp_inbox.values_mut() {
            if let Some(envelope) = deque.pop_front() {
                self.bump_version();
                return (Some(envelope), notify);
            }
        }

        (None, notify)
    }

    // FIXME: See recv
    pub(crate) fn recv_from(&mut self, peer: SocketAddr) -> (Option<Envelope>, Rc<Semaphore>) {
        let deque = self.udp_inbox.entry(peer).or_default();
        let notify = self.udp_notify.clone();

        match deque.pop_front() {
            Some(envelope) => {
                self.bump_version();
                (Some(envelope), notify)
            }
            _ => (None, notify),
        }
    }

    pub(crate) fn tick(&mut self, now: Instant) {
        self.now = now;
    }
}

/// Host software that implements the simulated Transmission Control Protocol
/// (TCP).
pub(crate) struct Tcp {
    /// Bound server sockets
    binds: IndexMap<u16, Inbox<PendingAccept>>,

    /// Active connections, keyed by (local, remote)
    connections: IndexMap<SocketPair, Inbox<Segment>>,
}

struct PendingAccept {
    src: SocketAddr,
    syn: Syn,
}

/// A simple unbounded channel.
struct Inbox<T> {
    /// Queued items
    deque: VecDeque<T>,

    /// Signaled when an item is available to recv.
    // We use a Semphore instead of [`tokio::sync::Notify`] because of this behavior:
    //
    // > If notify_one() is called multiple times before notified().await, only
    // a single permit is stored. The next call to notified().await will
    // complete immediately, but the one after will wait for a new permit.
    //
    // .. which complicates code in the receivers because it becomes bi-modal:
    //
    // - a message may be immediately available, so there is no need to wait
    // - or we have to wait
    notify: Rc<Semaphore>,
}

impl<T> Default for Inbox<T> {
    fn default() -> Self {
        Self {
            deque: VecDeque::new(),
            notify: Rc::new(Semaphore::new(0)),
        }
    }
}

impl Tcp {
    fn new() -> Self {
        Self {
            binds: IndexMap::new(),
            connections: IndexMap::new(),
        }
    }

    pub(crate) fn bind(&mut self, addr: SocketAddr) -> io::Result<Rc<Semaphore>> {
        use indexmap::map::Entry::Vacant;

        let notify = match self.binds.entry(addr.port()) {
            Vacant(e) => e.insert(Inbox::default()).notify.clone(),
            _ => return Err(io::Error::new(io::ErrorKind::AddrInUse, addr.to_string())),
        };

        Ok(notify)
    }

    pub(crate) fn accept(&mut self, addr: SocketAddr) -> Option<(TcpStream, SocketAddr)> {
        if let Some(PendingAccept { src, syn }) = self.binds[&addr.port()].deque.pop_front() {
            // Perform the SYN-ACK, returning early if the peer has hung up to
            // avoid mutations.
            syn.notify.send(()).ok()?;

            let pair = SocketPair(addr, src);
            self.new_stream(pair);
            let notify = self.finish_connect(pair);

            return Some((TcpStream::new(pair, notify), src));
        }

        None
    }

    pub(crate) fn new_stream(&mut self, pair: SocketPair) {
        let inbox = Inbox {
            deque: VecDeque::new(),
            notify: Rc::new(Semaphore::new(0)),
        };

        assert!(
            self.connections.insert(pair, inbox).is_none(),
            "pair is already connected {:?}",
            pair
        );
    }

    pub(crate) fn finish_connect(&mut self, pair: SocketPair) -> Rc<Semaphore> {
        self.connections[&pair].notify.clone()
    }

    pub(crate) fn receive_from_network(&mut self, envelope: Envelope) -> &Semaphore {
        match message::downcast(envelope.message) {
            Segment::Syn(syn) => {
                assert!(
                    !self
                        .connections
                        .contains_key(&SocketPair::new(syn.dst, envelope.src.host)),
                    "already connected!"
                );

                let binds = &mut self.binds[&syn.dst.port()];
                binds.deque.push_back(PendingAccept {
                    src: envelope.src.host,
                    syn,
                });

                &binds.notify
            }
            Segment::Data(it) => {
                let inbox = self.connections[&it.pair];
                inbox.deque.push_back(Segment::Data(it));
                &inbox.notify
            }
            _ => todo!("implement fin and rst"),
        }
    }
}
