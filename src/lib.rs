mod connection;
pub mod error;
pub mod listener;
mod packet;
mod socket;

// General idea of how we respond to packets:
//
// Inside the listener:
// - A remote peer sends us a UDP datagram containing a uTP packet.
// - We check a buffer or poll a future to get an incoming packet.
// - We deserialize the packet and examine its type.
//   - if it's a SYN packet, check to see if we already have a matching connection
//     - if we're already connected, ignore this packet
//     - else, begin the handshake process to set up a new connection. Channels to
//       connections are stored in a HashMap, identified by the 16-bit connection ID in
//       the packet header.
//   - if not SYN, route the packet to an existing connection, or send a RESET if there is
//     no existing connection.
//
// Inside a given connection:
// - We check a buffer or poll a future to get an incoming packet from the socket
// - If the connection ID doesn't match this connection, route it to the correct
//   connection
// - Handle the routed packet, updating any internal buffers and connection state
//
// General overview of architecture:
//
// Implement Stream<Connection> for UtpListener and Stream<Message> for Connection,
// returning Poll::Pending as quickly as possible if unable to make fast progress.
//
// Messages contain data from one or more packets.
//
// A Router holds a synchronized HashMap of connection IDs to channels, through which we
// can send packets to any connection.
//
// Each connection shares access to the underlying UtpSocket. When a connection wants to
// read a packet, it either checks its receiving channel or stores a future to read the
// socket, and polls it. If a packet is received, we check the connection ID field. If it
// doesn't match the current connection's ID, then we route the packet through the packet
// router, which sends the packet to the corresponding connection. When a connection is
// dropped, the router cannot send messages to it anymore and its entry in the HashMap is
// removed.
//
// To produce connections from a UtpListener: if the next packet is a SYN, initiate the
// handshake process by adding an entry to the packet router's DashMap, then return a new
// connection that has a pending future to write an ACK for the received SYN to the
// socket.
//
// To act as a client, we have to connect to a UTP server. What we can do is add a method
// to Connection that produces a connection to a remote socket. This connection would
// share ownership of a Router with other connections. The only difference from the
// server model is that there would be no need for a listener object. In practice,
// however, we usually have a server and a client on the same port, so we can simply
// register the new client connection to the existing router. They're all duplex
// connections anyway, so there's no functional difference between them.

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
