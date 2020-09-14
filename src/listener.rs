use std::{net::SocketAddr, sync::Arc};

use bytes::Bytes;
use tokio::net::ToSocketAddrs;

use crate::{
    connection::Connection,
    error::*,
    packet::{Packet, PacketType},
    socket::UtpSocket,
};

pub struct UtpListener {
    socket: Arc<UtpSocket>,
}

impl UtpListener {
    pub fn new(socket: Arc<UtpSocket>) -> Self {
        Self { socket }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr()
    }

    /// Creates a new UtpListener, which will be bound to the specified address.
    pub async fn bind(addr: impl ToSocketAddrs) -> Result<Self> {
        Ok(Self::new(Arc::new(UtpSocket::bind(addr).await?)))
    }

    pub async fn accept(&self) -> Result<Connection> {
        loop {
            let (packet, addr) = self.socket.get_syn().await?;
            if self
                .socket
                .init_connection(packet.connection_id)
                .await
                .is_ok()
            {
                // TODO: Craft valid state packet to respond to SYN
                #[rustfmt::skip]
                let _state_packet = Packet::new(
                    PacketType::State, 1, packet.connection_id,
                    0, 0, 0, 0, 0, vec![], Bytes::new(),
                );
                return Ok(Connection::new(
                    Arc::clone(&self.socket),
                    packet.connection_id,
                    addr,
                ));
            }
        }
    }
}
