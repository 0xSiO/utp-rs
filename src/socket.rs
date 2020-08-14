use std::{convert::TryFrom, net::SocketAddr};

use bytes::{Bytes, BytesMut};
use tokio::net::{ToSocketAddrs, UdpSocket};

use crate::{error::*, Packet};

// Ethernet MTU minus IP/UDP header sizes. TODO: Use path MTU discovery
const MAX_DATAGRAM_SIZE: usize = 1472;

pub struct UtpSocket {
    socket: UdpSocket,
    // Maximum number of bytes the socket may have in-flight at any given time
    // max_window: u32,
    // Number of bytes currently in-flight
    // local_window: u32,
    // Upper limit of the number of in-flight bytes, as given by remote peer
    // remote_window: u32,
}

impl UtpSocket {
    pub async fn bind(local_addr: impl ToSocketAddrs) -> Result<Self> {
        Ok(Self {
            socket: UdpSocket::bind(local_addr).await?,
        })
    }

    pub async fn connect(&self, peer_addr: impl ToSocketAddrs) -> Result<()> {
        Ok(self.socket.connect(peer_addr).await?)
    }

    pub async fn send(&mut self, packet: Packet) -> Result<usize> {
        Ok(self.socket.send(&Bytes::from(packet)).await?)
    }

    pub async fn send_to(&mut self, packet: Packet, target: impl ToSocketAddrs) -> Result<usize> {
        Ok(self.socket.send_to(&Bytes::from(packet), target).await?)
    }

    pub async fn recv(&mut self) -> Result<Packet> {
        let mut buf = BytesMut::with_capacity(MAX_DATAGRAM_SIZE);
        buf.resize(MAX_DATAGRAM_SIZE, 0);
        let bytes_read = self.socket.recv(&mut buf).await?;
        buf.truncate(bytes_read);
        Ok(Packet::try_from(buf.freeze())?)
    }

    pub async fn recv_from(&mut self) -> Result<(Packet, SocketAddr)> {
        let mut buf = BytesMut::with_capacity(MAX_DATAGRAM_SIZE);
        buf.resize(MAX_DATAGRAM_SIZE, 0);
        let (bytes_read, addr) = self.socket.recv_from(&mut buf).await?;
        buf.truncate(bytes_read);
        Ok((Packet::try_from(buf.freeze())?, addr))
    }
}
