use std::{collections::VecDeque, io, net::SocketAddr, sync::Arc};

use dashmap::{mapref::entry::Entry, DashMap};
use log::trace;
use tokio::{
    net::{ToSocketAddrs, UdpSocket},
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        Mutex,
    },
};

use crate::{error::ConnectionError, packet::Packet};

mod packet_receiver;
mod packet_sender;

use self::{packet_receiver::PacketReceiver, packet_sender::PacketSender};

// Ethernet MTU minus IP/UDP header sizes.
// TODO: Lower this limit a bit more, since things like VPNs will increase fragmentation
// TODO: Use path MTU discovery
pub(crate) const MAX_DATAGRAM_SIZE: usize = 1472;

// TODO: Maybe consider some good bounds for all the unbounded channels
pub struct UtpSocket {
    /// Underlying UDP socket.
    socket: Arc<UdpSocket>,
    /// Local address for underlying UDP socket.
    local_addr: SocketAddr,
    /// Internal routing table for connections listening on this socket. For each value in this
    /// map, a UtpStream will hold the corresponding receive half of the channel.
    routing_table: Arc<DashMap<(u16, SocketAddr), UnboundedSender<Packet>>>,
    // Receiver to allow UtpListeners to receive SYN packets
    syn_rx: Arc<Mutex<UnboundedReceiver<(Packet, SocketAddr)>>>,
    /// Sender for outgoing packets. UtpStreams can send packets through this sender by calling
    /// UtpSocket::send_to. Technically we could clone this and provide a sender to each UtpStream
    /// instead, but it's a bit more complex. TODO: Consider the cloning approach
    outgoing_tx: UnboundedSender<(Packet, SocketAddr)>,
}

impl UtpSocket {
    /// Create a new [`UtpSocket`] and attempt to bind it to the provided address.
    pub async fn bind(local_addr: impl ToSocketAddrs) -> io::Result<Self> {
        let udp_socket = UdpSocket::bind(local_addr).await?;
        trace!("binding to {}", udp_socket.local_addr()?);
        // This will spawn two IO tasks: one for reading incoming packets into the routing table, and
        // one for sending outgoing packets.
        Self::try_from(udp_socket)
    }

    fn spawn_sender(&self, outgoing_rx: UnboundedReceiver<(Packet, SocketAddr)>) {
        tokio::spawn(PacketSender {
            socket: Arc::downgrade(&self.socket),
            local_addr: self.local_addr,
            outgoing_rx,
            outgoing_buffer: VecDeque::new(),
        });
    }

    fn spawn_receiver(&self, syn_tx: UnboundedSender<(Packet, SocketAddr)>) {
        tokio::spawn(PacketReceiver {
            socket: Arc::downgrade(&self.socket),
            local_addr: self.local_addr,
            syn_tx,
            routing_table: Arc::downgrade(&self.routing_table),
        });
    }

    /// Return the local [`SocketAddr`] that this socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Send a [`Packet`] to a remote address.
    pub(crate) fn send_to(&self, packet: Packet, remote_addr: SocketAddr) -> io::Result<()> {
        // TODO: Error means that our sender task has been dropped
        self.outgoing_tx
            .send((packet, remote_addr))
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
    }

    /// Register a new connection in the socket routing table. A new connection ID will be
    /// generated for this entry and returned, along with the receiving end of a packet channel.
    pub(crate) fn register_connection(
        &self,
        remote_addr: SocketAddr,
    ) -> (u16, UnboundedReceiver<Packet>) {
        // TODO: Maybe timeout if this takes too long
        loop {
            let connection_id = rand::random::<u16>();
            // Note that the entry holds a dashmap::lock::RwLockWriteGuard on the relevant data
            match self.routing_table.entry((connection_id, remote_addr)) {
                Entry::Occupied(_) => continue,
                vacant => {
                    let (packet_tx, packet_rx) = mpsc::unbounded_channel();
                    let entry = vacant.or_insert(packet_tx);
                    debug_assert_eq!(entry.key(), &(connection_id, remote_addr));
                    return (connection_id, packet_rx);
                }
            }
        }
    }

    /// Insert a new connection in the socket routing table. The connection ID and remote address
    /// pair should not already exist in the routing table. The receiving end of a packet channel
    /// will be returned if the operation was successful.
    pub(crate) fn insert_connection(
        &self,
        connection_id: u16,
        remote_addr: SocketAddr,
    ) -> io::Result<UnboundedReceiver<Packet>> {
        // Note that the entry holds a dashmap::lock::RwLockWriteGuard on the relevant data
        match self.routing_table.entry((connection_id, remote_addr)) {
            Entry::Occupied(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                ConnectionError::AlreadyExists(connection_id, remote_addr),
            )),
            vacant => {
                let (packet_tx, packet_rx) = mpsc::unbounded_channel();
                let entry = vacant.or_insert(packet_tx);
                debug_assert_eq!(entry.key(), &(connection_id, remote_addr));
                Ok(packet_rx)
            }
        }
    }

    pub(crate) async fn get_syn(&self) -> Option<(Packet, SocketAddr)> {
        let mut lock = self.syn_rx.lock().await;
        lock.recv().await
    }
}

impl TryFrom<UdpSocket> for UtpSocket {
    type Error = io::Error;

    fn try_from(socket: UdpSocket) -> io::Result<Self> {
        let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel();
        let (syn_tx, syn_rx) = mpsc::unbounded_channel();
        let local_addr = socket.local_addr()?;

        let utp_socket = Self {
            socket: Arc::new(socket),
            local_addr,
            routing_table: Arc::new(DashMap::new()),
            syn_rx: Arc::new(Mutex::new(syn_rx)),
            outgoing_tx,
        };
        utp_socket.spawn_sender(outgoing_rx);
        utp_socket.spawn_receiver(syn_tx);

        Ok(utp_socket)
    }
}

impl TryFrom<std::net::UdpSocket> for UtpSocket {
    type Error = io::Error;

    fn try_from(socket: std::net::UdpSocket) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        Self::try_from(UdpSocket::try_from(socket)?)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;
    use crate::test_helper::*;

    #[tokio::test]
    async fn bind_test() {
        let socket = UtpSocket::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(socket.local_addr().ip(), Ipv4Addr::LOCALHOST);

        let socket = UtpSocket::bind("::1:0").await.unwrap();
        assert_eq!(socket.local_addr().ip(), Ipv6Addr::LOCALHOST);
    }

    #[tokio::test]
    async fn local_addr_test() {
        let socket = get_socket().await;
        assert_eq!(socket.local_addr(), socket.socket.local_addr().unwrap());
    }

    #[tokio::test]
    async fn send_and_recv_test() {
        let (socket_1, socket_2) = (get_socket().await, get_socket().await);
        let (connection_id, mut packet_rx) = socket_2.register_connection(socket_1.local_addr());

        let mut packet = get_packet();
        packet.connection_id = connection_id;

        socket_1
            .outgoing_tx
            .send((packet.clone(), socket_2.local_addr()))
            .unwrap();

        assert_eq!(packet_rx.recv().await, Some(packet));
    }

    #[tokio::test]
    async fn init_connection_test() {
        let socket = get_socket().await;
        let (connection_id, remote_addr) = (1, "127.0.0.2:8080".parse().unwrap());

        let mut receiver = socket
            .insert_connection(connection_id, remote_addr)
            .unwrap();

        assert!(receiver.try_recv().is_err());

        assert!(socket
            .insert_connection(connection_id, remote_addr)
            .is_err());
    }

    #[tokio::test]
    async fn register_connection_test() {
        let socket = get_socket().await;
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let (id_1, mut receiver_1) = socket.register_connection(addr);
        let (id_2, mut receiver_2) = socket.register_connection(addr);

        assert_ne!(id_1, id_2);

        assert!(receiver_1.try_recv().is_err());
        assert!(receiver_2.try_recv().is_err());

        // Add a packet to one queue. Other queue that we set up should be empty.
        socket
            .routing_table
            .get(&(id_1, addr))
            .unwrap()
            .send(get_packet())
            .unwrap();

        assert!(receiver_1.try_recv().is_ok());
        assert!(receiver_2.try_recv().is_err());
    }

    #[tokio::test]
    async fn conversions_test() {
        let std_udp =
            tokio::task::spawn_blocking(|| std::net::UdpSocket::bind("localhost:0").unwrap())
                .await
                .unwrap();
        assert!(UtpSocket::try_from(std_udp).is_ok());

        let tokio_udp = tokio::net::UdpSocket::bind("localhost:0").await.unwrap();
        assert!(UtpSocket::try_from(tokio_udp).is_ok());
    }
}
