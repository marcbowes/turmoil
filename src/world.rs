use crate::net::{Segment, SocketPair, Syn};
use crate::{
    config, message, net::TcpStream, version, Dns, Envelope, Host, Log, Message, ToSocketAddr,
    Topology,
};

use indexmap::IndexMap;
use rand::RngCore;
use scoped_tls::scoped_thread_local;
use std::cell::RefCell;
use std::net::{IpAddr, SocketAddr};
use std::rc::Rc;
use tokio::sync::{oneshot, Notify};
use tokio::time::Instant;

/// Tracks all the state for the simulated world.
pub(crate) struct World {
    /// Tracks all individual hosts
    pub(crate) hosts: IndexMap<SocketAddr, Host>,

    /// Tracks how each host is connected to each other.
    pub(crate) topology: Topology,

    /// Maps hostnames to socket addresses.
    pub(crate) dns: Dns,

    /// If set, this is the current host being executed.
    pub(crate) current: Option<SocketAddr>,

    /// Handle to the logger
    pub(crate) log: Log,

    /// Random number generator used for all decisions. To make execution
    /// determinstic, reuse the same seed.
    rng: Box<dyn RngCore>,
}

scoped_thread_local!(static CURRENT: RefCell<World>);

impl World {
    /// Initialize a new world.
    pub(crate) fn new(link: config::Link, log: Log, rng: Box<dyn RngCore>) -> World {
        World {
            hosts: IndexMap::new(),
            topology: Topology::new(link),
            dns: Dns::new(),
            current: None,
            log,
            rng,
        }
    }

    /// Run `f` on the world.
    pub(crate) fn current<R>(f: impl FnOnce(&mut World) -> R) -> R {
        CURRENT.with(|current| {
            let mut current = current.borrow_mut();
            f(&mut *current)
        })
    }

    /// Run `f` if the world is set - otherwise no-op.
    ///
    /// Used in drop paths, where the simulation may be shutting
    /// down and we don't need to do anything.
    pub(crate) fn current_if_set(f: impl FnOnce(&mut World) -> ()) {
        if CURRENT.is_set() {
            Self::current(f);
        }
    }

    pub(crate) fn enter<R>(world: &RefCell<World>, f: impl FnOnce() -> R) -> R {
        CURRENT.set(world, f)
    }

    /// Return a reference to the currently executing host.
    pub(crate) fn current_host(&self) -> &Host {
        let addr = self.current.expect("current host missing");
        self.hosts.get(&addr).expect("host missing")
    }

    /// Return a reference to the host at `addr`.
    pub(crate) fn host(&self, addr: SocketAddr) -> &Host {
        self.hosts.get(&addr).expect("host missing")
    }

    /// Return a mutable reference to the host at `addr`.
    pub(crate) fn host_mut(&mut self, addr: SocketAddr) -> &mut Host {
        self.hosts.get_mut(&addr).expect("host missing")
    }

    pub(crate) fn lookup(&mut self, host: impl ToSocketAddr) -> SocketAddr {
        self.dns.lookup(host)
    }

    pub(crate) fn hold(&mut self, a: IpAddr, b: IpAddr) {
        self.topology.hold(a, b);
    }

    pub(crate) fn release(&mut self, a: IpAddr, b: IpAddr) {
        self.topology.release(a, b);
    }

    pub(crate) fn partition(&mut self, a: IpAddr, b: IpAddr) {
        self.topology.partition(a, b);
    }

    pub(crate) fn repair(&mut self, a: IpAddr, b: IpAddr) {
        self.topology.repair(a, b);
    }

    /// Register a new host with the simulation.
    pub(crate) fn register(&mut self, addr: SocketAddr, epoch: Instant, notify: Rc<Notify>) {
        assert!(
            !self.hosts.contains_key(&addr),
            "already registered host for the given socket address"
        );

        // Register links between the new host and all existing hosts
        for existing in self.hosts.keys() {
            self.topology.register(existing.ip(), addr.ip());
        }

        // Initialize host state
        self.hosts.insert(addr, Host::new(addr, epoch, notify));
    }

    /// Initiate a new connection with `dst` from the currently executing host.
    pub(crate) fn connect(&mut self, dst: SocketAddr) -> (oneshot::Receiver<()>, SocketPair) {
        let (sender, receiver) = oneshot::channel();
        let syn = Syn {
            dst,
            notify: sender,
        };

        let current_addr = self.current_host().addr;
        let host = self.host_mut(current_addr);

        let local: SocketAddr = (current_addr.ip(), host.assign_ephemeral_port()).into();
        let pair = SocketPair::new(local, dst);

        host.tcp.new_stream(pair);

        self.topology
            .enqueue_message(&mut self.rng, local, dst, Box::new(Segment::Syn(syn)));

        (receiver, pair)
    }

    /// Accept an incoming connection on the currently executing host.
    pub(crate) fn accept(&mut self, addr: SocketAddr) -> Option<(TcpStream, SocketAddr)> {
        let current_addr = self.current_host().addr;
        let ret = self.host_mut(current_addr).tcp.accept(addr);

        if ret.is_some() {
            // TODO: Log
        }

        ret
    }

    /// Send `message` to `dst` from the current host. Delivery is asynchronous
    /// and not guaranteed.
    pub(crate) fn send_message(&mut self, dst: SocketAddr, message: Box<dyn Message>) {
        let src = self.current_host().addr;
        self.topology
            .enqueue_message(&mut self.rng, src, dst, message);
    }

    /// Receive a message on the currently executing host.
    pub(crate) fn recv(&mut self) -> (Option<Envelope>, Rc<Notify>) {
        let addr = self.current_host().addr;
        let host = &mut self.hosts[&addr];
        let ret = host.recv();

        if let Some(Envelope { src, message, .. }) = &ret.0 {
            self.log
                .recv(&self.dns, host.dot(), host.elapsed(), *src, &**message);
        }

        ret
    }

    /// Receive a message on the currently executing host from a `peer`.
    pub(crate) fn recv_from(&mut self, peer: SocketAddr) -> (Option<Envelope>, Rc<Notify>) {
        let addr = self.current_host().addr;
        let host = &mut self.hosts[&addr];
        let ret = host.recv_from(peer);

        if let Some(Envelope { src, message, .. }) = &ret.0 {
            self.log
                .recv(&self.dns, host.dot(), host.elapsed(), *src, &**message);
        }

        ret
    }

    // /// Receive a segment from `pair`'s peer on the currently executing host.
    // pub(crate) fn recv_on(&mut self, pair: SocketPair) -> Option<Segment> {
    //     let host = &mut self.hosts[&pair.local.host];
    //     let ret = host.recv_on(pair);

    //     if let Some(seg) = &ret {
    //         self.log
    //             .recv(&self.dns, host.dot(), host.elapsed(), pair.peer, seg);
    //     }

    //     ret
    // }

    /// Tick the host at `addr` to `now`.
    pub(crate) fn tick(&mut self, addr: SocketAddr, now: Instant) {
        self.hosts.get_mut(&addr).expect("missing host").tick(now);
    }
}
