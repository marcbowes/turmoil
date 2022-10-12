use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    rc::Rc,
};

use tokio::sync::Notify;

use crate::{world::World, ToSocketAddr};

use super::TcpStream;

const ANY: Ipv4Addr = Ipv4Addr::new(0, 0, 0, 0);

/// A simulated socket server, listening for connections.
///
/// All methods must be called from a host within a Turmoil simulation.
pub struct TcpListener {
    addr: SocketAddr,
    notify: Rc<Notify>,
}

impl TcpListener {
    /// Creates a new TcpListener, which will be bound to the specified address.
    ///
    /// The returned listener is ready for accepting connections.
    ///
    /// Currently, only IPv4 0.0.0.0 is supported.
    pub async fn bind<A: ToSocketAddr>(addr: A) -> io::Result<Self> {
        let (addr, notify) = World::current(|world| {
            let addr = match world.lookup(addr) {
                SocketAddr::V4(v4) => {
                    if v4.ip() != &ANY {
                        panic!("{} is not supported", v4)
                    }
                    SocketAddr::V4(v4)
                }
                _ => unimplemented!("IPv6 is not implemented"),
            };

            let current_addr = world.current_host().addr;
            let ret = world.host_mut(current_addr).tcp.bind(addr);

            if ret.is_ok() {
                // TODO: Log
            }

            (addr, ret)
        });

        Ok(Self {
            addr,
            notify: notify?,
        })
    }

    /// Accepts a new incoming connection from this listener.
    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        loop {
            let maybe_accept = World::current(|world| world.accept(self.addr));

            if let Some(accept) = maybe_accept {
                return Ok(accept);
            }

            self.notify.notified().await;
        }
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        World::current_if_set(|world| {
            // TODO: unbind
        })
    }
}
