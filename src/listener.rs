use std::{io, net::SocketAddr, sync::Arc};

use bytes::Bytes;
use tokio::net::ToSocketAddrs;

use crate::{
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
    pub async fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
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
    pub async fn accept(&self) -> io::Result<UtpStream> {
        // TODO: Getting None means the send half of the SYN channel has been dropped
        let (packet, remote_addr) = self.socket.get_syn().await.unwrap();
        let connection_id_recv = packet.connection_id.wrapping_add(1);
        let connection_id_send = packet.connection_id;
        let seq_number = rand::random::<u16>();
        let ack_number = packet.seq_number;

        // state: SYN received

        if let Ok(receiver) = self
            .socket
            .insert_connection(connection_id_recv, remote_addr)
        {
            // TODO: Move this into UtpStream functionality
            #[rustfmt::skip]
            let syn_ack = Packet::new(
                PacketType::State, 1, connection_id_send, 0, 0, 0, seq_number, ack_number,
                vec![], Bytes::new(),
            );
            let seq_number = seq_number.wrapping_add(1);
            self.socket.send_to(syn_ack, remote_addr)?;

            // TODO: We aren't technically 'connected' until we start receiving data packets

            Ok(UtpStream::new(
                Arc::clone(&self.socket),
                connection_id_recv,
                connection_id_send,
                remote_addr,
                receiver,
                seq_number,
                ack_number,
                Default::default(),
                Default::default(),
                Default::default(),
            ))
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

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;
    use crate::test_helper::*;

    #[tokio::test]
    async fn bind_test() {
        let listener = UtpListener::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(listener.socket.local_addr().ip(), Ipv4Addr::LOCALHOST);

        let listener = UtpListener::bind("::1:0").await.unwrap();
        assert_eq!(listener.socket.local_addr().ip(), Ipv6Addr::LOCALHOST);
    }

    #[tokio::test]
    async fn local_addr_test() {
        let listener = get_listener().await;
        assert_eq!(listener.local_addr(), listener.socket.local_addr());
    }

    #[tokio::test]
    async fn accept_test() {
        let listener = get_listener().await;
        let local_socket = Arc::clone(&listener.socket);
        let remote_socket = Arc::new(get_socket().await);

        let (conn_id, mut remote_receiver) =
            remote_socket.register_connection(local_socket.local_addr());

        let mut packet = get_packet();
        packet.packet_type = PacketType::Syn;
        packet.connection_id = conn_id;

        remote_socket
            .send_to(packet.clone(), local_socket.local_addr())
            .unwrap();

        let stream = listener.accept().await.unwrap();
        assert_eq!(stream.connection_id_send(), packet.connection_id);
        assert_eq!(
            stream.connection_id_recv(),
            packet.connection_id.wrapping_add(1)
        );
        assert_eq!(stream.local_addr(), local_socket.local_addr());
        assert_eq!(stream.remote_addr(), remote_socket.local_addr());

        // SYN-ACK should have been sent to remote socket
        let syn_ack = remote_receiver.recv().await.unwrap();
        assert_eq!(syn_ack.packet_type, PacketType::State);
        assert_eq!(syn_ack.connection_id, stream.connection_id_send());
        assert_eq!(syn_ack.ack_number, packet.seq_number);
    }
}
