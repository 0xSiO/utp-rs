mod connection;
pub mod error;
pub mod listener;
mod packet;
mod router;
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
//       connections are stored in a DashMap, identified by the 16-bit connection ID in
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
// A Router holds a DashMap of connection IDs to channels, through which we can send
// packets to any connection.
//
// Each connection shares access to the underlying UtpSocket. When a connection wants to
// read a packet, it either checks its receiving channel or stores a future to read the
// socket, and polls it. If a packet is received, we check the connection ID field. If it
// doesn't match the current connection's ID, then we route the packet through the packet
// router, which sends the packet to the corresponding connection. When a connection is
// dropped, it removes its channel entry from the router.
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
    use bytes::Bytes;
    use futures_util::stream::StreamExt;

    use super::*;
    use listener::UtpListener;
    use packet::{Packet, PacketType};
    use socket::UtpSocket;

    #[tokio::test]
    async fn basic_connection_test() {
        let task = tokio::spawn(async {
            let mut server = UtpListener::bind("localhost:5000").await.unwrap();
            let conn = server.next().await.unwrap();
            assert!(conn.is_ok());
        });
        #[rustfmt::skip]
        let syn = Packet::new(PacketType::Syn, 1, 10, 20, 0, 30, 1, 0, vec![], Bytes::new());
        let socket = UtpSocket::bind("localhost:5001").await.unwrap();
        socket.send_to(syn, "localhost:5000").await.unwrap();
        task.await.unwrap();
    }
}
