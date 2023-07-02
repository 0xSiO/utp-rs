#![warn(rust_2018_idioms)]

mod congestion;
pub mod error;
mod listener;
mod packet;
mod socket;
mod stream;
mod time;

#[cfg(test)]
mod test_helper;

pub use crate::{listener::UtpListener, socket::UtpSocket, stream::UtpStream};

// General overview of architecture:
//
// A UtpSocket has the ability to send and receive packets through a UDP socket. Any outgoing
// packets are queued to be sent later in a background IO task. Incoming packets are read by
// another background IO task, and then grouped by (connection ID, remote addr) to be routed to
// each connection through an internal routing table. SYN packets are saved in a separate queue, to
// be read by a UtpListener.
//
// A UtpListener waits for incoming SYN packets from the UtpSocket, and attempts to create a
// UtpStream using the information in the packet.
//
// A UtpStream represents a connection to a remote peer. It can be created on a given UtpSocket,
// and has the ability to send data to and receive data from its peer. Connections are registered
// in the UtpSocket's routing table, where a (connection ID, remote addr) pair maps to the send
// half of a channel. The UtpStream holds the receive half of the channel (a.k.a. the "mailbox"),
// allowing it to receive packets asynchronously for that (connection ID, remote addr) pair.
//
// When attempting to write data to the UtpStream, the data will be broken into packets and queued
// to be sent to the peer via one of the UtpSocket's background IO tasks. The stream is not
// considered flushed until all the sent data has been ACKed by the remote peer.
//
// When attempting to read data from the UtpStream, the UtpStream will read and ACK as many packets
// as it can from its mailbox, then return as much data as it can from the received packets.
//
// UtpListeners and UtpStreams can share ownership of a single UtpSocket, so it's possible to have
// any number of clients and servers all running on the same socket.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use futures_util::{stream::FuturesUnordered, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::{
        packet::{Packet, PacketType},
        stream::MAX_DATA_SEGMENT_SIZE,
    };

    fn init_logger() {
        let _ = pretty_env_logger::try_init();
    }

    async fn get_socket() -> Arc<UtpSocket> {
        Arc::new(UtpSocket::bind("localhost:0").await.unwrap())
    }

    async fn get_connection_pair(
        socket_1: Arc<UtpSocket>,
        socket_2: Arc<UtpSocket>,
    ) -> (UtpStream, UtpStream) {
        let listener = UtpListener::new(Arc::clone(&socket_2));
        let conn_1 = tokio::spawn(async move {
            UtpStream::connect(socket_1, socket_2.local_addr())
                .await
                .unwrap()
        });
        let conn_2 = tokio::spawn(async move { listener.accept().await.unwrap() });
        let (conn_1, conn_2) = tokio::join!(conn_1, conn_2);
        (conn_1.unwrap(), conn_2.unwrap())
    }

    #[tokio::test]
    async fn basic_connection_test() {
        init_logger();

        let socket_1 = get_socket().await;
        let socket_2 = get_socket().await;
        let (conn_1, conn_2) = get_connection_pair(socket_1, socket_2).await;

        assert_eq!(conn_1.connection_id_send(), conn_2.connection_id_recv());
        assert_eq!(conn_1.connection_id_recv(), conn_2.connection_id_send());
    }

    #[tokio::test]
    async fn routing_test() {
        init_logger();

        let local_socket = get_socket().await;
        let remote_socket = get_socket().await;

        // Make this smaller if your operating system doesn't have large enough socket buffers
        const MAX_CONNS: usize = 200;

        let local_conns = (0..MAX_CONNS)
            .map(|_| {
                let local_socket = Arc::clone(&local_socket);
                let remote_socket = Arc::clone(&remote_socket);
                // Just grab the first connection in the pair since we'll be checking it later
                async move { get_connection_pair(local_socket, remote_socket).await.0 }
            })
            .collect::<FuturesUnordered<_>>()
            .collect::<Vec<UtpStream>>()
            .await;

        let send_tasks = (0..MAX_CONNS)
            .map(|i| {
                let remote_socket = Arc::clone(&remote_socket);
                let local_addr = local_socket.local_addr();
                let connection_id_recv = local_conns[i].connection_id_recv();
                async move {
                    #[rustfmt::skip]
                    remote_socket
                        .send_to(
                            Packet::new(PacketType::State, 1, connection_id_recv, 20, 0, 30,
                                        1, 0, vec![], Bytes::new()),
                            local_addr,
                        )
                        .unwrap();
                }
            })
            .collect::<FuturesUnordered<_>>();

        let recv_tasks = local_conns
            .into_iter()
            .map(|mut conn| async move {
                let packet = conn.inbound_packets().recv().await.unwrap();
                assert_eq!(packet.connection_id, conn.connection_id_recv());
            })
            .collect::<FuturesUnordered<_>>();

        let send_handle = tokio::spawn(send_tasks.collect::<Vec<_>>());
        let recv_handle = tokio::spawn(recv_tasks.collect::<Vec<_>>());
        let _ = tokio::join!(send_handle, recv_handle);
    }

    #[tokio::test]
    async fn async_read_and_write_test() {
        init_logger();

        let local_socket = get_socket().await;
        let remote_socket = get_socket().await;

        let (mut stream_1, mut stream_2) =
            get_connection_pair(Arc::clone(&local_socket), Arc::clone(&remote_socket)).await;

        // Send/receive 1 packet of data
        let message = [1_u8; MAX_DATA_SEGMENT_SIZE];
        stream_1.write_all(&message).await.unwrap();
        let mut buf = [0; MAX_DATA_SEGMENT_SIZE];
        let ((), bytes_read) =
            tokio::try_join!(stream_1.flush(), stream_2.read_exact(&mut buf)).unwrap();
        assert_eq!(bytes_read, message.len());
        assert_eq!(buf, message);

        // Send/receive multiple packets of data
        const NUM_PACKETS: usize = 25;
        // Extra amount of bytes to force a smaller packet to be sent
        const LEFTOVER: usize = 512;
        let large_message = [1_u8; MAX_DATA_SEGMENT_SIZE * NUM_PACKETS + LEFTOVER];
        stream_1.write_all(&large_message).await.unwrap();
        let mut large_buf = [0; MAX_DATA_SEGMENT_SIZE * NUM_PACKETS + LEFTOVER];
        let ((), bytes_read) =
            tokio::try_join!(stream_1.flush(), stream_2.read_exact(&mut large_buf)).unwrap();
        assert_eq!(bytes_read, large_message.len());
        assert_eq!(large_buf, large_message);
    }
}
