#![warn(rust_2018_idioms)]

pub mod error;
mod listener;
mod packet;
mod socket;
mod stream;

#[cfg(test)]
mod test_helper;

pub use crate::{listener::UtpListener, socket::UtpSocket, stream::UtpStream};

// General overview of architecture:
//
// A UtpSocket has the ability to send and receive packets through a UDP socket. Incoming
// packets are queued and grouped by (connection ID, remote addr) into a routing table.
// UtpStreams can request packets for a given connection ID, and a UtpListener can request
// SYN packets from a separate queue in the UtpSocket.
//
// Given a UtpSocket, a connection to a remote socket can be created by requesting a new
// entry in the routing table.
//
// UtpStreams and UtpListeners share ownership of the UtpSocket, so we can have a client
// configuration, a server configuration, or both configurations at once.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use futures_util::{stream::FuturesUnordered, StreamExt, TryStreamExt};
    use log::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use packet::{Packet, PacketType};
    use stream::MAX_DATA_SEGMENT_SIZE;

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
        const MAX_CONNS: usize = 250;

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
                    let result = remote_socket
                        .send_to(
                            Packet::new(PacketType::State, 1, connection_id_recv, 20, 0, 30,
                                        1, 0, vec![], Bytes::new()),
                            local_addr,
                        )
                        .await;
                    if result.unwrap() != 20 {
                        error!("Didn't send 20 bytes");
                    }
                }
            })
            .collect::<FuturesUnordered<_>>();

        let recv_tasks = local_conns
            .into_iter()
            .map(|conn| {
                let local_socket = Arc::clone(&local_socket);
                let remote_socket = Arc::clone(&remote_socket);
                async move {
                    let packet = local_socket
                        .packets(conn.connection_id_recv(), remote_socket.local_addr())
                        .try_next()
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(packet.connection_id, conn.connection_id_recv());
                }
            })
            .collect::<FuturesUnordered<_>>();

        let send_handle = tokio::spawn(send_tasks.collect::<Vec<_>>());
        let recv_handle = tokio::spawn(recv_tasks.collect::<Vec<_>>());
        let _ = tokio::join!(send_handle, recv_handle);
    }

    #[tokio::test]
    async fn data_and_ack_test() {
        init_logger();

        let local_socket = get_socket().await;
        let remote_socket = get_socket().await;

        let (stream_1, stream_2) =
            get_connection_pair(Arc::clone(&local_socket), Arc::clone(&remote_socket)).await;

        debug!("starting test");

        #[rustfmt::skip]
        let data_packet = Packet::new(PacketType::Data, 1, stream_1.connection_id_send(), 0, 0, 0, 2,
                                 123, vec![], Bytes::from_static(&[1, 2, 3, 4, 5]));
        local_socket
            .send_to(data_packet.clone(), remote_socket.local_addr())
            .await
            .unwrap();

        let received_packet = remote_socket
            .packets(stream_2.connection_id_recv(), stream_2.remote_addr())
            .try_next()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received_packet.seq_number, data_packet.seq_number);

        #[rustfmt::skip]
        let ack_packet = Packet::new(PacketType::State, 1, stream_2.connection_id_send(), 0, 0, 0,
                                     124, 2, vec![], Bytes::new());
        remote_socket
            .send_to(ack_packet.clone(), local_socket.local_addr())
            .await
            .unwrap();

        let received_packet = local_socket
            .packets(stream_1.connection_id_recv(), stream_1.remote_addr())
            .try_next()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received_packet.seq_number, ack_packet.seq_number);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_read_and_write_test() {
        init_logger();

        let local_socket = get_socket().await;
        let remote_socket = get_socket().await;

        let (mut stream_1, stream_2) =
            get_connection_pair(Arc::clone(&local_socket), Arc::clone(&remote_socket)).await;

        // Send/receive 1 packet of data
        let message = [1_u8; 5];
        stream_1.write_all(&message).await.unwrap();
        debug!("starting tasks");
        let write_task = tokio::spawn(async move { stream_1.flush().await.unwrap() });
        let read_task = tokio::spawn(async move {
            let packet = remote_socket
                .packets(stream_2.connection_id_recv(), stream_2.remote_addr())
                .try_next()
                .await
                .unwrap()
                .unwrap();

            assert_eq!(packet.data.as_ref(), &message);

            // Somehow this works
            #[rustfmt::skip]
            let ack_packet = Packet::new(PacketType::State, 1, stream_2.connection_id_send(), 0, 0,
                                         0, packet.ack_number + 1, packet.seq_number, vec![], Bytes::new());

            remote_socket
                .send_to(ack_packet, stream_2.remote_addr())
                .await
                .unwrap();
        });
        let ((), ()) = tokio::try_join!(write_task, read_task).unwrap();

        //     // Send/receive multiple packets of data
        //     const NUM_PACKETS: usize = 4;
        //     let large_message = [1_u8; MAX_DATA_SEGMENT_SIZE * NUM_PACKETS];
        //     stream_1.write_all(&large_message).await.unwrap();
        //     let mut large_buf = [0; MAX_DATA_SEGMENT_SIZE * NUM_PACKETS];
        //     let ((), bytes_read) =
        //         tokio::try_join!(stream_1.flush(), stream_2.read_exact(&mut large_buf)).unwrap();
        //     assert_eq!(bytes_read, large_message.len());
        //     assert_eq!(large_buf, large_message);
    }
}
