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
    use futures_util::{future::join_all, io::AsyncWriteExt};
    use log::*;

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

        // TODO: 279 simultaneous connections seems to stall the test. Figure out why
        const MAX_CONNS: u16 = 278;

        let local_conns: Vec<UtpStream> = join_all((0..MAX_CONNS).into_iter().map(|_: u16| {
            tokio::spawn(UtpStream::connect(
                Arc::clone(&local_socket),
                remote_socket.local_addr(),
            ))
        }))
        .await
        .into_iter()
        .map(|result| result.unwrap().unwrap())
        .collect();

        #[rustfmt::skip]
        let packets: Vec<Packet> = (0..MAX_CONNS).into_iter().map(|i| {
            Packet::new(PacketType::State, 1, local_conns[i as usize].connection_id(),
                        20, 0, 30, 1, 0, vec![], Bytes::new())
        }).collect();

        let send_task = join_all((0..MAX_CONNS).into_iter().map(|i: u16| {
            let remote_socket = Arc::clone(&remote_socket);
            let local_addr = local_socket.local_addr();
            let packet = packets[i as usize].clone();
            tokio::spawn(async move {
                let result = remote_socket.send_to(packet, local_addr).await;
                assert!(result.unwrap() > 0);
            })
        }));

        let recv_task = join_all(local_conns.into_iter().map(|conn| {
            tokio::spawn(async move {
                match conn.recv().await {
                    Ok(result) => assert_eq!(result, ()),
                    Err(err) => error!("{}", err),
                }
            })
        }));

        let _ = tokio::join!(send_task, recv_task);
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
}
