use std::{
    convert::TryFrom,
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use crossbeam_queue::SegQueue;
use dashmap::{mapref::entry::Entry, DashMap};
use futures_util::{ready, stream::Stream};
use log::{debug, trace};
use tokio::{
    io::ReadBuf,
    net::{lookup_host, ToSocketAddrs, UdpSocket},
};

use crate::{
    error::*,
    packet::{Packet, PacketType},
};

// Ethernet MTU minus IP/UDP header sizes. TODO: Use path MTU discovery
pub(crate) const MAX_DATAGRAM_SIZE: usize = 1472;

#[derive(Debug)]
pub struct UtpSocket {
    socket: UdpSocket,
    packet_queues: DashMap<(u16, SocketAddr), SegQueue<Packet>>,
    syn_packets: SegQueue<(Packet, SocketAddr)>,
    local_addr: SocketAddr,
    // Maximum number of bytes the socket may have in-flight at any given time
    // max_window: u32,
    // Number of bytes currently in-flight
    // local_window: u32,
    // Upper limit of the number of in-flight bytes, as given by remote peer
    // remote_window: u32,
}

impl UtpSocket {
    fn new(
        socket: UdpSocket,
        packet_queues: DashMap<(u16, SocketAddr), SegQueue<Packet>>,
        syn_packets: SegQueue<(Packet, SocketAddr)>,
        local_addr: SocketAddr,
    ) -> Self {
        Self {
            socket,
            packet_queues,
            syn_packets,
            local_addr,
        }
    }

    /// Create a new [`UtpSocket`] and attempt to bind it to the provided address.
    pub async fn bind(local_addr: impl ToSocketAddrs) -> io::Result<Self> {
        let udp_socket = UdpSocket::bind(local_addr).await?;
        let local_addr = udp_socket.local_addr()?;
        trace!("binding to {}", local_addr);
        Ok(Self::new(
            udp_socket,
            Default::default(),
            Default::default(),
            local_addr,
        ))
    }

    /// Return the local [`SocketAddr`] that this socket is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Send a [`Packet`] to a remote address.
    // TODO: Use futures_util::futures::poll_fn?
    pub async fn send_to(
        &self,
        packet: Packet,
        remote_addr: impl ToSocketAddrs,
    ) -> io::Result<usize> {
        let remote_addr = lookup_host(remote_addr)
            .await?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, Error::MissingAddress))?;
        debug!(
            "Conn #{}: {} -> {} {:?}",
            packet.connection_id, self.local_addr, remote_addr, packet.packet_type
        );
        // TODO: Update packet timing data
        Ok(self
            .socket
            .send_to(&Bytes::from(packet), remote_addr)
            .await?)
    }

    // TODO: This method assumes that the packet will be sent with one successful call to poll_send_to, i.e.
    //       the number of bytes written to the socket will equal the number of bytes in the packet
    pub fn poll_send_to(
        &self,
        cx: &mut Context<'_>,
        packet: Packet,
        remote_addr: SocketAddr,
    ) -> Poll<io::Result<usize>> {
        let datagram = Bytes::from(packet.clone());
        let bytes_written = ready!(self.socket.poll_send_to(cx, &datagram, remote_addr))?;
        debug!(
            "Conn #{}: {} -> {} {:?} ({} bytes)",
            packet.connection_id, self.local_addr, remote_addr, packet.packet_type, bytes_written
        );
        debug_assert_eq!(bytes_written, datagram.len());
        Poll::Ready(Ok(bytes_written))
    }

    /// Receive a [`Packet`] from a remote address.
    pub async fn recv_from(&self) -> io::Result<(Packet, SocketAddr)> {
        let mut buf = BytesMut::with_capacity(MAX_DATAGRAM_SIZE);
        buf.resize(MAX_DATAGRAM_SIZE, 0);
        let (bytes_read, remote_addr) = self.socket.recv_from(&mut buf).await?;
        buf.truncate(bytes_read);
        let packet = Packet::try_from(buf.freeze())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        debug!(
            "Conn #{}: {} <- {} {:?} ({} bytes)",
            packet.connection_id, self.local_addr, remote_addr, packet.packet_type, bytes_read
        );
        Ok((packet, remote_addr))
    }

    fn poll_recv_from(&self, cx: &mut Context<'_>) -> Poll<io::Result<(Packet, SocketAddr)>> {
        let mut buf = [0; MAX_DATAGRAM_SIZE];
        let mut buf = ReadBuf::new(&mut buf);
        let remote_addr = ready!(self.socket.poll_recv_from(cx, &mut buf))?;
        let packet = Packet::try_from(Bytes::copy_from_slice(buf.filled()))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        debug!(
            "Conn #{}: {} <- {} {:?} ({} bytes)",
            packet.connection_id,
            self.local_addr,
            remote_addr,
            packet.packet_type,
            buf.filled().len()
        );
        Poll::Ready(Ok((packet, remote_addr)))
    }

    /// Route a [`Packet`] from a remote address to the correct packet queue for the connection
    /// registered in the routing table.
    fn route_packet(&self, packet: Packet, remote_addr: SocketAddr) {
        match self.packet_queues.get(&(packet.connection_id, remote_addr)) {
            Some(queue) => queue.push(packet),
            None => debug!(
                "no connections found for packet from {}: {:?}",
                remote_addr, packet
            ),
        }
    }

    /// Fetch a SYN packet from either the dedicated SYN packet buffer or wait until the underlying
    /// socket receives a SYN packet. Any other packets are routed to their respective connections.
    pub(crate) async fn get_syn(&self) -> io::Result<(Packet, SocketAddr)> {
        loop {
            if let Some(packet_and_addr) = self.syn_packets.pop() {
                return Ok(packet_and_addr);
            }

            let (packet, remote_addr) = self.recv_from().await?;
            if let PacketType::Syn = packet.packet_type {
                return Ok((packet, remote_addr));
            } else {
                self.route_packet(packet, remote_addr);
            }
        }
    }

    /// Initialize a new connection in the socket routing table. The connection ID and remote
    /// address pair should not already exist in the routing table.
    pub(crate) fn init_connection(
        &self,
        connection_id: u16,
        remote_addr: SocketAddr,
    ) -> io::Result<()> {
        // Note that the entry holds a dashmap::lock::RwLockWriteGuard on the relevant data
        match self.packet_queues.entry((connection_id, remote_addr)) {
            Entry::Occupied(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                Error::ConnectionExists(connection_id, remote_addr),
            )),
            Entry::Vacant(entry) => {
                let entry = entry.insert(Default::default());
                // Add some assertions just to be sure this completed correctly
                debug_assert_eq!(entry.key(), &(connection_id, remote_addr));
                debug_assert!(entry.value().is_empty());
                Ok(())
            }
        }
    }

    /// Register a new connection in the socket routing table. A new connection ID will be
    /// generated for this entry and returned.
    pub(crate) fn register_connection(&self, remote_addr: SocketAddr) -> u16 {
        // TODO: Maybe timeout if this takes too long
        loop {
            let connection_id = rand::random::<u16>();
            // Note that the entry holds a dashmap::lock::RwLockWriteGuard on the relevant data
            match self.packet_queues.entry((connection_id, remote_addr)) {
                Entry::Occupied(_) => continue,
                vacant => {
                    let entry = vacant.or_default();
                    // Add some assertions just to be sure this completed correctly
                    debug_assert_eq!(entry.key(), &(connection_id, remote_addr));
                    debug_assert!(entry.value().is_empty());
                    return connection_id;
                }
            }
        }
    }

    /// Return a stream of [`Packet`]s from this socket intended for the provided connection ID and
    /// remote address.
    pub(crate) fn packets(
        &self,
        connection_id: u16,
        remote_addr: SocketAddr,
    ) -> impl Stream<Item = io::Result<Packet>> + '_ {
        PacketStream {
            socket: self,
            connection_id,
            remote_addr,
        }
    }
}

impl TryFrom<UdpSocket> for UtpSocket {
    type Error = io::Error;

    fn try_from(socket: UdpSocket) -> io::Result<Self> {
        let local_addr = socket.local_addr()?;
        Ok(UtpSocket::new(
            socket,
            Default::default(),
            Default::default(),
            local_addr,
        ))
    }
}

impl TryFrom<std::net::UdpSocket> for UtpSocket {
    type Error = io::Error;

    fn try_from(socket: std::net::UdpSocket) -> io::Result<Self> {
        Self::try_from(UdpSocket::try_from(socket)?)
    }
}

struct PacketStream<'s> {
    socket: &'s UtpSocket,
    connection_id: u16,
    remote_addr: SocketAddr,
}

impl<'s> PacketStream<'s> {
    fn poll_next_priv(&self, cx: &mut Context<'_>) -> Poll<Option<io::Result<Packet>>> {
        if let Some(queue) = self
            .socket
            .packet_queues
            .get(&(self.connection_id, self.remote_addr))
        {
            if let Some(packet) = queue.pop() {
                return Poll::Ready(Some(Ok(packet)));
            }
        } else {
            // TODO: Simplify this if statement if the else branch is never called
            unreachable!();
        }

        if let Poll::Ready(result) = self.socket.poll_recv_from(cx) {
            let (packet, actual_addr) = result?;

            if let PacketType::Syn = packet.packet_type {
                self.socket.syn_packets.push((packet, actual_addr));
            } else {
                if (packet.connection_id, actual_addr) == (self.connection_id, self.remote_addr) {
                    return Poll::Ready(Some(Ok(packet)));
                } else {
                    self.socket.route_packet(packet, actual_addr);
                }
            }
        }

        // Didn't get a packet, so schedule to be polled again
        // TODO: If this results in high CPU usage, consider re-thinking this packet stream
        //       architecture. Maybe use a single task to poll the socket and wait for data using
        //       channels.
        cx.waker().wake_by_ref();
        return Poll::Pending;
    }
}

impl<'s> Stream for PacketStream<'s> {
    type Item = io::Result<Packet>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll_next_priv(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, Ipv6Addr},
        sync::Arc,
    };

    use futures_util::TryStreamExt;

    use super::*;
    use crate::test_helper::*;

    #[tokio::test]
    async fn bind_test() {
        let socket = UtpSocket::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(
            socket.socket.local_addr().unwrap().ip(),
            Ipv4Addr::LOCALHOST
        );

        let socket = UtpSocket::bind("::1:0").await.unwrap();
        assert_eq!(
            socket.socket.local_addr().unwrap().ip(),
            Ipv6Addr::LOCALHOST
        );
    }

    #[tokio::test]
    async fn local_addr_test() {
        let socket = get_socket().await;
        assert_eq!(socket.local_addr(), socket.socket.local_addr().unwrap());
    }

    #[tokio::test]
    async fn send_to_test() {
        let (sender, receiver) = (get_socket().await, get_socket().await);

        assert_eq!(
            sender
                .send_to(
                    get_packet(),
                    format!("localhost:{}", receiver.local_addr().port()),
                )
                .await
                .unwrap(),
            Bytes::from(get_packet()).len()
        );

        assert!(sender
            .send_to(get_packet(), "does not exist")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn recv_from_test() {
        let (sender, receiver) = (get_socket().await, get_socket().await);
        sender
            .send_to(get_packet(), receiver.local_addr())
            .await
            .unwrap();

        assert_eq!(
            receiver.recv_from().await.unwrap(),
            (get_packet(), sender.local_addr())
        );
    }

    #[tokio::test]
    async fn route_packet_test() {
        let socket = get_socket().await;
        let packet = get_packet();
        let connection_id = packet.connection_id;
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

        // Create a queue in the routing table for the packet to end up in
        socket.init_connection(connection_id, addr).unwrap();

        socket.route_packet(packet.clone(), addr);
        assert_eq!(
            socket
                .packet_queues
                .get(&(connection_id, addr))
                .unwrap()
                .len(),
            1
        );

        assert_eq!(
            socket
                .packet_queues
                .get(&(connection_id, addr))
                .unwrap()
                .pop(),
            Some(packet)
        );
    }

    #[tokio::test]
    async fn get_syn_test() {
        let (sender, receiver) = (get_socket().await, get_socket().await);
        let mut packet = get_packet();
        packet.packet_type = PacketType::Syn;

        // Receive from SYN packet queue

        receiver
            .syn_packets
            .push((packet.clone(), sender.local_addr()));

        assert_eq!(
            receiver.get_syn().await.unwrap(),
            (packet.clone(), sender.local_addr())
        );

        // Wait for SYN using recv_from

        let expected_packet = packet.clone();
        let sender_addr = sender.local_addr();
        let receiver_addr = receiver.local_addr();

        let recv_handle = tokio::spawn(async move {
            assert_eq!(
                receiver.get_syn().await.unwrap(),
                (expected_packet, sender_addr)
            );
        });

        tokio::spawn(async move {
            sender.send_to(packet.clone(), receiver_addr).await.unwrap();
        });

        recv_handle.await.unwrap();
    }

    #[tokio::test]
    async fn init_connection_test() {
        let socket = get_socket().await;
        let (connection_id, remote_addr) = (1, "127.0.0.2:8080".parse().unwrap());

        socket.init_connection(connection_id, remote_addr).unwrap();

        assert!(socket
            .packet_queues
            .get(&(connection_id, remote_addr))
            .unwrap()
            .is_empty());

        assert!(socket.init_connection(connection_id, remote_addr).is_err());
    }

    #[tokio::test]
    async fn register_connection_test() {
        let socket = get_socket().await;
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let id_1 = socket.register_connection(addr);
        let id_2 = socket.register_connection(addr);

        assert_ne!(id_1, id_2);

        assert!(socket.packet_queues.get(&(id_1, addr)).unwrap().is_empty());

        // Add a packet to this queue. Other queue that we set up should be empty.
        socket
            .packet_queues
            .get(&(id_1, addr))
            .unwrap()
            .push(get_packet());

        assert!(socket.packet_queues.get(&(id_2, addr)).unwrap().is_empty());
    }

    #[tokio::test]
    async fn packet_stream_test() {
        let (sender, receiver) = (Arc::new(get_socket().await), Arc::new(get_socket().await));
        let packet = get_packet();

        // Receive packet from existing packet queue

        receiver
            .init_connection(packet.connection_id, sender.local_addr())
            .unwrap();
        receiver
            .packet_queues
            .get(&(packet.connection_id, sender.local_addr()))
            .unwrap()
            .push(packet.clone());

        assert_eq!(
            receiver
                .packets(packet.connection_id, sender.local_addr())
                .try_next()
                .await
                .unwrap(),
            Some(packet.clone())
        );

        // Receive packet using poll_recv_from

        sender
            .send_to(packet.clone(), receiver.local_addr())
            .await
            .unwrap();

        let packet_clone = packet.clone();
        let sender_clone = Arc::clone(&sender);
        let receiver_clone = Arc::clone(&receiver);

        let recv_handle = tokio::spawn(async move {
            assert_eq!(
                receiver_clone
                    .packets(packet_clone.connection_id, sender_clone.local_addr())
                    .try_next()
                    .await
                    .unwrap(),
                Some(packet_clone)
            );
        });

        recv_handle.await.unwrap();

        // Route SYN packets to the SYN packet queue

        let packet_clone = packet.clone();
        let mut syn_packet = packet.clone();
        syn_packet.packet_type = PacketType::Syn;
        let sender_clone = Arc::clone(&sender);
        let receiver_clone = Arc::clone(&receiver);

        tokio::spawn(async move {
            sender_clone
                .send_to(syn_packet, receiver_clone.local_addr())
                .await
                .unwrap();
            sender_clone
                .send_to(packet_clone, receiver_clone.local_addr())
                .await
                .unwrap();
        });

        let packet_clone = packet.clone();
        let sender_clone = Arc::clone(&sender);
        let receiver_clone = Arc::clone(&receiver);

        let recv_handle = tokio::spawn(async move {
            assert_eq!(
                receiver_clone
                    .packets(packet_clone.connection_id, sender_clone.local_addr())
                    .try_next()
                    .await
                    .unwrap(),
                Some(packet_clone)
            );

            // Should have a SYN packet waiting for us
            assert!(receiver_clone.get_syn().await.is_ok());
        });

        recv_handle.await.unwrap();
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
