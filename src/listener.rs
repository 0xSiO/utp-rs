use std::sync::Arc;

use crossbeam_deque::Injector;
use dashmap::DashMap;
use tokio::{
    net::ToSocketAddrs,
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        Mutex,
    },
};

use crate::{
    error::*,
    packet::{Packet, PacketType},
    UtpSocket,
};

// General idea of how this might work:
//
// Inside the listener:
// - A remote peer sends us a UDP datagram containing a uTP packet.
// - ??? (some structure or task needs to read incoming packets)
// - We deserialize the packet and examine its type.
//   - if it's a SYN packet, check to see if we already have a matching connection
//     - if we're already connected, ignore this packet
//     - else, begin the handshake process to set up a new connection. Channels to
//       connections are stored in a HashMap, identified by the 16-bit connection ID in
//       the packet header.
//   - if not SYN, route the packet to an existing connection, or send a RESET if there is
//     no existing connection.
//
// Inside a given connection:
// - ??? (some structure or task needs to read the routed packet)
// - handle the routed packet, updating any internal buffers and connection state

// New plan!
//
// Each connection shares access to the socket. When a connection reads a packet, it
// checks the connection ID. If it doesn't match the current connection's ID, then
// it sends the packet through the corresponding connection's incoming channel. We locate
// the channel using a DashMap. When a connection is dropped, it removes its channel
// entry from the DashMap.

pub struct UtpListener {
    socket: Arc<Mutex<UtpSocket>>,
    connection_queue: Injector<Connection>,
    incoming_packet_tx_map: DashMap<u16, UnboundedSender<Packet>>,
    outbound_packet_tx: UnboundedSender<Packet>,
    outbound_packet_rx: UnboundedReceiver<Packet>,
}

impl UtpListener {
    /// Creates a new UtpListener, which will be bound to the specified address.
    ///
    /// The returned listener is ready to begin listening for incoming messages.
    ///
    /// Binding with a port number of 0 will request that the OS assigns a port to this
    /// listener. The port allocated can be queried via the local_addr method.
    ///
    /// If addr yields multiple addresses, bind will be attempted with each of the
    /// addresses until one succeeds and returns the listener. If none of the addresses
    /// succeed in creating a listener, the error returned from the last attempt (the last
    /// address) is returned.
    pub async fn bind(addr: impl ToSocketAddrs) -> Result<Self> {
        let (outbound_packet_tx, outbound_packet_rx) = unbounded_channel();
        Ok(UtpListener {
            socket: Arc::new(Mutex::new(UtpSocket::bind(addr).await?)),
            connection_queue: Injector::new(),
            incoming_packet_tx_map: Default::default(),
            outbound_packet_tx,
            outbound_packet_rx,
        })
    }

    // TODO: Need a way to spawn this loop but also allow accepting new connections
    pub async fn listen(&self) -> Result<()> {
        loop {
            let packet = self.socket.lock().await.recv().await?;

            match packet.packet_type {
                PacketType::Syn => {
                    if let Some(_) = self.incoming_packet_tx_map.get(&packet.connection_id) {
                        // Connection ID collides with existing connection, so ignore
                        continue;
                    }

                    let (new_incoming_tx, new_incoming_rx) = unbounded_channel();
                    self.incoming_packet_tx_map
                        .insert(packet.connection_id, new_incoming_tx.clone());
                    self.connection_queue.push(Connection::new(
                        ConnectionState::Pending,
                        self.outbound_packet_tx.clone(),
                        new_incoming_rx,
                    ));

                    // TODO: Negotiate new connection with client
                    new_incoming_tx.send(packet).unwrap();
                }
                _ => todo!(),
            }
        }
    }
}

enum ConnectionState {
    Pending,
    Established,
}

struct Connection {
    state: ConnectionState,
    outbound_packet_tx: UnboundedSender<Packet>,
    incoming_packet_rx: UnboundedReceiver<Packet>,
}

impl Connection {
    pub fn new(
        state: ConnectionState,
        outbound_packet_tx: UnboundedSender<Packet>,
        incoming_packet_rx: UnboundedReceiver<Packet>,
    ) -> Self {
        Self {
            state,
            outbound_packet_tx,
            incoming_packet_rx,
        }
    }
}
