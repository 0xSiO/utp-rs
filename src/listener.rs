use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use tokio::{net::ToSocketAddrs, sync::Mutex};

use crate::{error::*, UtpSocket};

// General idea of how this might work:
//
// Inside the listener:
// - A remote peer sends us a UDP datagram containing a uTP packet.
// - ??? (some structure or task needs to read incoming packets)
// - We deserialize the packet and examine its type.
//   - if it's a SYN packet, check to see if we already have a matching connection
//     - if we're already connected, ignore this packet
//     - else, begin the handshake process to set up a new connection. Connections are
//       stored in a HashMap, identified by the 16-bit connection ID in the packet header.
//   - if not SYN, route the packet to an existing connection, or send a RESET if there is
//     no existing connection.
//
// Inside a given connection:
// - ??? (some structure or task needs to read the routed packet)
// - handle the routed packet, updating any internal buffers and connection state

struct UtpListener {
    socket: Arc<Mutex<UtpSocket>>,
    connections: HashMap<u16, Connection>,
}

impl UtpListener {
    /// Creates a new UtpListener, which will be bound to the specified address.
    ///
    /// The returned listener is ready for accepting connections.
    ///
    /// Binding with a port number of 0 will request that the OS assigns a port to this
    /// listener. The port allocated can be queried via the local_addr method.
    ///
    /// If addr yields multiple addresses, bind will be attempted with each of the
    /// addresses until one succeeds and returns the listener. If none of the addresses
    /// succeed in creating a listener, the error returned from the last attempt (the last
    /// address) is returned.
    pub async fn bind(addr: impl ToSocketAddrs) -> Result<Self> {
        Ok(UtpListener {
            socket: Arc::new(Mutex::new(UtpSocket::bind(addr).await?)),
            connections: Default::default(),
        })
    }

    pub async fn accept(&mut self) -> Result<(UtpStream, SocketAddr)> {
        todo!()
    }
}

struct UtpStream {
    connection: Connection,
}

enum ConnectionState {
    Pending,
    Established,
}

struct Connection {
    socket: Arc<Mutex<UtpSocket>>,
    state: ConnectionState,
}
