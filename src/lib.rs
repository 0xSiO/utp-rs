mod connection;
pub mod error;
pub mod listener;
mod packet;
mod socket;

// General overview of architecture:
//
// A UtpSocket has the ability to send and receive packets through a UDP socket. Incoming
// packets are queued and grouped by the connection ID field into a routing table.
// Connections can request packets for a given connection ID, and a UtpListener can
// request SYN packets from a separate queue in the UtpSocket.
//
// Given a UtpSocket, a connection to a remote socket can be created by requesting a new
// entry in the routing table.
//
// Connections and UtpListeners share ownership of the UtpSocket, so we can have a client
// configuration, a server configuration, or both configurations at once.

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use bytes::Bytes;
    use futures_util::future::join_all;
    use log::error;

    use super::*;
    use connection::Connection;
    use listener::UtpListener;
    use packet::{Packet, PacketType};
    use socket::UtpSocket;

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
        let listener_addr = listener.local_addr();
        let task = tokio::spawn(async move {
            let result = tokio::time::timeout(Duration::from_millis(500), listener.accept()).await;
            match result {
                Ok(Ok(_conn)) => {} // TODO: Check that conn is valid
                Ok(Err(err)) => panic!("encountered error: {}", err),
                Err(_) => {} // read timed out, probably due to packet loss
            }
        });
        #[rustfmt::skip]
        let syn = Packet::new(PacketType::Syn, 1, 10, 20, 0, 30, 1, 0, vec![], Bytes::new());
        let socket = get_socket().await;
        socket.send_to(syn, listener_addr).await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn routing_test() {
        init_logger();

        let local_socket = Arc::new(get_socket().await);
        let remote_socket = Arc::new(get_socket().await);

        // TODO: More than 278 hangs the test... figure out why
        const MAX_CONNS: u16 = 278;

        let local_conns: Vec<Connection> = join_all((0..MAX_CONNS).into_iter().map(|_: u16| {
            Connection::generate(Arc::clone(&local_socket), remote_socket.local_addr())
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
            join_all(
                local_conns
                    .iter()
                    .map(|conn| tokio::time::timeout(Duration::from_millis(500), conn.recv())),
            )
            .await
            .into_iter()
            .for_each(|result| match result {
                Ok(result) => assert_eq!(result.unwrap(), ()),
                Err(err) => error!("{}", err),
            });
        });

        let _ = tokio::join!(send_task, recv_task);
    }
}
