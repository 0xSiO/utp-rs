use std::{convert::TryFrom, net::SocketAddr};

use bytes::{Bytes, BytesMut};
use log::{debug, trace};
use tokio::{
    net::{ToSocketAddrs, UdpSocket},
    sync::Mutex,
};

use crate::{error::*, packet::Packet, util::resolve};

// Ethernet MTU minus IP/UDP header sizes. TODO: Use path MTU discovery
const MAX_DATAGRAM_SIZE: usize = 1472;

#[derive(Debug)]
pub struct UtpSocket {
    socket: Mutex<UdpSocket>,
    local_addr: SocketAddr,
    // Maximum number of bytes the socket may have in-flight at any given time
    // max_window: u32,
    // Number of bytes currently in-flight
    // local_window: u32,
    // Upper limit of the number of in-flight bytes, as given by remote peer
    // remote_window: u32,
}

impl UtpSocket {
    pub fn new(socket: Mutex<UdpSocket>, local_addr: SocketAddr) -> Self {
        Self { socket, local_addr }
    }

    pub async fn bind(local_addr: impl ToSocketAddrs) -> Result<Self> {
        let local_addr = resolve(local_addr).await?;
        trace!("binding to {}", local_addr);
        Ok(Self::new(
            Mutex::new(UdpSocket::bind(local_addr).await?),
            local_addr,
        ))
    }

    pub async fn send_to(&self, packet: Packet, remote_addr: impl ToSocketAddrs) -> Result<usize> {
        let remote_addr = resolve(remote_addr).await?;
        debug!(
            "{} sending {:?} to {}",
            self.local_addr, packet.packet_type, remote_addr
        );
        Ok(self
            .socket
            .lock()
            .await
            .send_to(&Bytes::from(packet), remote_addr)
            .await?)
    }

    pub async fn recv_from(&self) -> Result<(Packet, SocketAddr)> {
        let mut buf = BytesMut::with_capacity(MAX_DATAGRAM_SIZE);
        buf.resize(MAX_DATAGRAM_SIZE, 0);
        let (bytes_read, remote_addr) = self.socket.lock().await.recv_from(&mut buf).await?;
        buf.truncate(bytes_read);
        let packet = Packet::try_from(buf.freeze())?;
        debug!(
            "{} got {:?} from {}, {} bytes",
            self.local_addr, packet.packet_type, remote_addr, bytes_read
        );
        Ok((packet, remote_addr))
    }
}
