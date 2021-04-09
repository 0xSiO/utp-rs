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

    /// Create a new UtpListener and attempt to bind it to the provided address.
    pub async fn bind(addr: impl ToSocketAddrs) -> Result<Self> {
        Ok(Self::new(Arc::new(UtpSocket::bind(addr).await?)))
    }

    /// Return the local address that this listener is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr()
    }

    /// Accept a connection from a remote socket, returning a [`UtpStream`].
    ///
    /// This will first wait for a `Syn` packet from the socket, then initialize a connection in
    /// the socket's routing table. If that is successful, a `State` packet will be sent back to
    /// the remote socket and a new stream will be created.
    pub async fn accept(&self) -> Result<UtpStream> {
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
            todo!(
                "Failed to initialize connection. Perhaps one exists already in the routing table?"
            );
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
