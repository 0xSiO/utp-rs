#![warn(rust_2018_idioms)]

pub mod error;
mod listener;
mod packet;
mod socket;
mod stream;

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
    use futures_util::stream::{FuturesUnordered, StreamExt, TryStreamExt};
    use log::*;
    use tokio::io::AsyncWriteExt;

    use super::*;
    use packet::{Packet, PacketType};

    fn init_logger() {
        let _ = pretty_env_logger::try_init();
    }

    async fn get_socket() -> UtpSocket {
        UtpSocket::bind("localhost:0").await.unwrap()
    }

    async fn get_listener() -> UtpListener {
        UtpListener::bind("localhost:0").await.unwrap()
    }

    #[tokio::test]
    async fn basic_connection_test() {
        init_logger();

        let listener = get_listener().await;

        #[rustfmt::skip]
        let syn = Packet::new(PacketType::Syn, 1, 10, 20, 0, 30, 1, 0, vec![], Bytes::new());
        let socket = get_socket().await;
        socket.send_to(syn, listener.local_addr()).await.unwrap();

        let conn = listener.accept().await.unwrap();
        assert_eq!(conn.local_addr(), listener.local_addr());
        assert_eq!(conn.remote_addr(), socket.local_addr());
    }

    #[tokio::test]
    async fn routing_test() {
        init_logger();

        let local_socket = Arc::new(get_socket().await);
        let remote_socket = Arc::new(get_socket().await);

        // Make this smaller if your operating system doesn't have large enough socket buffers
        const MAX_CONNS: u16 = 250;

        let conns: Result<Vec<UtpStream>, _> = (0..MAX_CONNS)
            .into_iter()
            .map(|_: u16| UtpStream::connect(Arc::clone(&local_socket), remote_socket.local_addr()))
            .collect::<FuturesUnordered<_>>()
            .try_collect()
            .await;
        let local_conns = conns.unwrap();

        #[rustfmt::skip]
        let packets: Vec<Packet> = (0..MAX_CONNS).into_iter().map(|i| {
            Packet::new(PacketType::State, 1, local_conns[i as usize].connection_id(),
                        20, 0, 30, 1, 0, vec![], Bytes::new())
        }).collect();

        let send_tasks = (0..MAX_CONNS)
            .into_iter()
            .map(|i: u16| {
                let remote_socket = Arc::clone(&remote_socket);
                let local_addr = local_socket.local_addr();
                let packet = packets[i as usize].clone();
                async move {
                    let result = remote_socket.send_to(packet, local_addr).await;
                    if result.unwrap() != 20 {
                        error!("Didn't send 20 bytes");
                    }
                }
            })
            .collect::<FuturesUnordered<_>>();

        let recv_tasks = local_conns
            .into_iter()
            .map(|mut conn| async move {
                match conn.recv().await {
                    Ok(result) => assert_eq!(result, ()),
                    Err(err) => error!("{}", err),
                }
            })
            .collect::<FuturesUnordered<_>>();

        let send_handle = tokio::spawn(send_tasks.collect::<Vec<_>>());
        let recv_handle = tokio::spawn(recv_tasks.collect::<Vec<_>>());
        let _ = tokio::join!(send_handle, recv_handle);
    }

    #[tokio::test]
    async fn async_write_test() {
        init_logger();

        let local_socket = Arc::new(get_socket().await);
        let remote_socket = Arc::new(get_socket().await);

        let local_addr = local_socket.local_addr();
        let remote_addr = remote_socket.local_addr();

        let mut stream = UtpStream::connect(local_socket, remote_addr).await.unwrap();

        let message = &[1_u8; crate::stream::MAX_DATA_SEGMENT_SIZE];
        stream.write_all(message).await.unwrap();
        stream.flush().await.unwrap();
        let (packet, addr) = remote_socket.recv_from().await.unwrap();
        assert_eq!(packet.data.len(), message.len());
        assert_eq!(packet.data.as_ref(), message);
        assert_eq!(addr, local_addr);

        const NUM_PACKETS: usize = 4;
        let large_message = &[1_u8; crate::stream::MAX_DATA_SEGMENT_SIZE * NUM_PACKETS];
        stream.write_all(large_message).await.unwrap();
        stream.flush().await.unwrap();

        let mut result: Vec<u8> = Vec::with_capacity(large_message.len());
        for _ in 0..NUM_PACKETS {
            let (packet, _) = remote_socket.recv_from().await.unwrap();
            result.extend(packet.data);
        }
        assert_eq!(result.len(), large_message.len());
        assert_eq!(result, large_message);
    }

    #[tokio::test]
    async fn ack_test() {
        init_logger();

        let local_socket = Arc::new(get_socket().await);
        let remote_socket = Arc::new(get_socket().await);

        let local_addr = local_socket.local_addr();
        let remote_addr = remote_socket.local_addr();

        // Send some data to the remote socket
        let mut stream_1 = UtpStream::connect(Arc::clone(&local_socket), remote_addr)
            .await
            .unwrap();
        let message = &[1_u8; crate::stream::MAX_DATA_SEGMENT_SIZE];
        stream_1.write_all(message).await.unwrap();
        stream_1.flush().await.unwrap();

        // Check that we successfully received data on the remote socket
        let mut stream_2 = UtpStream::connect(remote_socket, local_addr).await.unwrap();
        assert!(stream_2.recv().await.is_ok());

        // Check that the remote socket sent us back a State packet
        let (packet, _) = local_socket.recv_from().await.unwrap();
        assert_eq!(packet.packet_type, PacketType::State);
    }
}
