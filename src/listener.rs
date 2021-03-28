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
    pub(crate) fn new(socket: Arc<UtpSocket>) -> Self {
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
            let (packet, remote_addr) = self.socket.get_syn().await?;
            let connection_id_recv = packet.connection_id.wrapping_add(1);
            let connection_id_send = packet.connection_id;
            let seq_number = rand::random::<u16>();
            let ack_number = packet.seq_number;

            // state: SYN received

            if self
                .socket
                .init_connection(connection_id_recv, remote_addr)
                .is_ok()
            {
                #[rustfmt::skip]
                let syn_ack = Packet::new(
                    PacketType::State, 1, connection_id_send, 0, 0, 0, seq_number, ack_number,
                    vec![], Bytes::new(),
                );
                let seq_number = seq_number.wrapping_add(1);
                self.socket.send_to(syn_ack, remote_addr).await?;

                // TODO: We aren't technically 'connected' until we start receiving data packets

                return Ok(UtpStream::new(
                    Arc::clone(&self.socket),
                    connection_id_recv,
                    connection_id_send,
                    remote_addr,
                    seq_number,
                    ack_number,
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    Default::default(),
                ));
            } else {
                todo!("Failed to initialize connection. Perhaps one exists already in the routing table?");
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
