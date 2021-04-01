use std::{
    collections::{hash_map::Entry, HashMap},
    convert::TryFrom,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::RwLock,
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use crossbeam_queue::SegQueue;
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
    packet_queues: RwLock<HashMap<(u16, SocketAddr), SegQueue<Packet>>>,
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
        packet_queues: RwLock<HashMap<(u16, SocketAddr), SegQueue<Packet>>>,
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

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

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

    fn route_packet(&self, packet: Packet, remote_addr: SocketAddr) {
        match self
            .packet_queues
            .read()
            .unwrap()
            .get(&(packet.connection_id, remote_addr))
        {
            Some(queue) => queue.push(packet),
            None => debug!(
                "no connections found for packet from {}: {:?}",
                remote_addr, packet
            ),
        }
    }

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

    pub(crate) fn init_connection(
        &self,
        connection_id: u16,
        remote_addr: SocketAddr,
    ) -> io::Result<()> {
        match self
            .packet_queues
            .write()
            .unwrap()
            .entry((connection_id, remote_addr))
        {
            Entry::Occupied(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                Error::ConnectionExists(connection_id, remote_addr),
            )),
            vacant => {
                vacant.or_default();
                Ok(())
            }
        }
    }

    pub(crate) fn register_connection(&self, remote_addr: SocketAddr) -> u16 {
        let mut states = self.packet_queues.write().unwrap();
        let mut connection_id = rand::random::<u16>();
        // TODO: Maybe timeout if this takes too long
        while states.contains_key(&(connection_id, remote_addr)) {
            connection_id = rand::random::<u16>();
        }
        debug_assert!(states
            .insert((connection_id, remote_addr), Default::default())
            .is_none());

        connection_id
    }

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
            .read()
            .unwrap()
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
