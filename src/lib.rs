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
    use futures_util::future::join_all;
    use log::error;

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

    #[tokio::test(core_threads = 2)]
    async fn routing_test() {
        init_logger();

        let local_socket = Arc::new(get_socket().await);
        let remote_socket = Arc::new(get_socket().await);

        // TODO: A very high limit here appears to stall the tokio runtime. For the
        // single-threaded scheduler on my machine, the limit is 278 simultaneous
        // connections. For 2 core_threads, the limit varies, but I've seen it get as high
        // as 800 on occasion.
        const MAX_CONNS: u16 = 300;

        let local_conns: Vec<UtpStream> = join_all((0..MAX_CONNS).into_iter().map(|_: u16| {
            UtpStream::connect(Arc::clone(&local_socket), remote_socket.local_addr())
        }))
        .await
        .into_iter()
        .map(|result| result.unwrap())
        .collect();

        #[rustfmt::skip]
        let packets: Vec<Packet> = (0..MAX_CONNS).into_iter().map(|i| {
            Packet::new(PacketType::State, 1, local_conns[i as usize].connection_id(),
                        20, 0, 30, 1, 0, vec![], Bytes::new())
        }).collect();

        let send_task = tokio::spawn(async move {
            join_all((0..MAX_CONNS).into_iter().map(|i: u16| {
                remote_socket.send_to(packets[i as usize].clone(), local_socket.local_addr())
            }))
            .await
            .into_iter()
            .for_each(|result| assert!(result.unwrap() > 0));
        });

        let recv_task = tokio::spawn(async move {
            join_all(local_conns.iter().map(|conn| conn.recv()))
                .await
                .into_iter()
                .for_each(|result| match result {
                    Ok(result) => assert_eq!(result, ()),
                    Err(err) => error!("{}", err),
                });
        });

        let _ = tokio::join!(send_task, recv_task);
    }
}
