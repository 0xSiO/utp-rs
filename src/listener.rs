use std::{net::SocketAddr, sync::Arc};

use bytes::Bytes;
use tokio::net::ToSocketAddrs;

use crate::{
    error::*,
    packet::{Packet, PacketType},
    socket::UtpSocket,
    stream::UtpStream,
};

pub struct UtpListener {
    socket: Arc<UtpSocket>,
}

impl UtpListener {
    fn new(socket: Arc<UtpSocket>) -> Self {
        Self { socket }
    }

    /// Creates a new UtpListener, which will be bound to the specified address.
    pub async fn bind(addr: impl ToSocketAddrs) -> Result<Self> {
        Ok(Self::new(Arc::new(UtpSocket::bind(addr).await?)))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr()
    }

    pub async fn accept(&self) -> Result<UtpStream> {
        loop {
            let (packet, addr) = self.socket.get_syn().await?;
            if self
                .socket
                .init_connection(packet.connection_id, addr)
                .is_ok()
            {
                // TODO: Craft valid state packet to respond to SYN
                #[rustfmt::skip]
                let _state_packet = Packet::new(
                    PacketType::State, 1, packet.connection_id,
                    0, 0, 0, 0, 0, vec![], Bytes::new(),
                );
                return Ok(UtpStream::new(
                    Arc::clone(&self.socket),
                    packet.connection_id,
                    addr,
                    // TODO: Queue up a STATE to send
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    Default::default(),
                ));
            }
        }
    }
}

impl From<UtpSocket> for UtpListener {
    fn from(socket: UtpSocket) -> Self {
        Self::new(Arc::new(socket))
    }
}

impl From<Arc<UtpSocket>> for UtpListener {
    fn from(socket: Arc<UtpSocket>) -> Self {
        Self::new(Arc::clone(&socket))
    }
}
