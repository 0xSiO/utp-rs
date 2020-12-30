use std::{
    collections::{hash_map::Entry, HashMap},
    convert::TryFrom,
    io,
    net::SocketAddr,
    sync::RwLock,
};

use bytes::{Bytes, BytesMut};
use crossbeam_queue::SegQueue;
use log::{debug, trace};
use tokio::net::{lookup_host, ToSocketAddrs, UdpSocket};

use crate::{
    error::*,
    packet::{Packet, PacketType},
};

// Ethernet MTU minus IP/UDP header sizes. TODO: Use path MTU discovery
const MAX_DATAGRAM_SIZE: usize = 1472;

// TODO: Add a way to bulk-write a list of packets
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
            "{} -> {} {:?}",
            self.local_addr, remote_addr, packet.packet_type
        );
        Ok(self
            .socket
            .send_to(&Bytes::from(packet), remote_addr)
            .await?)
    }

    pub async fn recv_from(&self) -> Result<(Packet, SocketAddr)> {
        let mut buf = BytesMut::with_capacity(MAX_DATAGRAM_SIZE);
        buf.resize(MAX_DATAGRAM_SIZE, 0);
        let (bytes_read, remote_addr) = self.socket.recv_from(&mut buf).await?;
        buf.truncate(bytes_read);
        let packet = Packet::try_from(buf.freeze())?;
        debug!(
            "{} <- {} {:?} ({} bytes)",
            self.local_addr, remote_addr, packet.packet_type, bytes_read
        );
        Ok((packet, remote_addr))
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

    pub(crate) async fn get_syn(&self) -> Result<(Packet, SocketAddr)> {
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
    ) -> Result<()> {
        match self
            .packet_queues
            .write()
            .unwrap()
            .entry((connection_id, remote_addr))
        {
            Entry::Occupied(_) => Err(Error::ConnectionExists(connection_id, remote_addr)),
            vacant => {
                vacant.or_default();
                Ok(())
            }
        }
    }

    pub(crate) fn register_connection(&self, remote_addr: SocketAddr) -> Result<u16> {
        let mut states = self.packet_queues.write().unwrap();
        let mut connection_id = 0;
        while states.contains_key(&(connection_id, remote_addr)) {
            connection_id = connection_id
                .checked_add(1)
                .ok_or_else(|| Error::TooManyConnections)?;
        }
        debug_assert!(states
            .insert((connection_id, remote_addr), Default::default())
            .is_none());
        Ok(connection_id)
    }

    pub(crate) async fn get_packet(
        &self,
        connection_id: u16,
        remote_addr: SocketAddr,
    ) -> Result<Packet> {
        loop {
            if let Some(queue) = self
                .packet_queues
                .read()
                .unwrap()
                .get(&(connection_id, remote_addr))
            {
                if let Some(packet) = queue.pop() {
                    return Ok(packet);
                }
            }

            let (packet, actual_addr) = self.recv_from().await?;
            if let PacketType::Syn = packet.packet_type {
                self.syn_packets.push((packet, actual_addr));
            } else {
                if packet.connection_id == connection_id {
                    if actual_addr == remote_addr {
                        return Ok(packet);
                    }
                    debug!(
                        "connection {} expected packet from {}, got one from {}",
                        connection_id, remote_addr, actual_addr
                    );
                } else {
                    self.route_packet(packet, actual_addr);
                }
            }
        }
    }
}

impl TryFrom<UdpSocket> for UtpSocket {
    type Error = Error;

    fn try_from(socket: UdpSocket) -> Result<Self> {
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
    type Error = Error;

    fn try_from(socket: std::net::UdpSocket) -> Result<Self> {
        Self::try_from(UdpSocket::try_from(socket)?)
    }
}
