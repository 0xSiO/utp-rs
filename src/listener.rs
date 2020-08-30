use std::{
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures_util::{future::LocalBoxFuture, ready, stream::Stream};
use tokio::{
    net::ToSocketAddrs,
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver},
        Mutex,
    },
};

use crate::{
    connection::Connection,
    connection_manager::{ConnectionManager, ConnectionState},
    error::*,
    packet::{Packet, PacketType},
    socket::UtpSocket,
};

// General idea of how this might work:
//
// Inside the listener:
// - A remote peer sends us a UDP datagram containing a uTP packet.
// - ??? (some structure or task needs to read incoming packets)
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
// - ??? (some structure or task needs to read the routed packet)
// - handle the routed packet, updating any internal buffers and connection state

// New plan!
//
// Implement Stream for listener and connection, returning Poll::Pending as quickly as
// possible if unable to make fast progress.
//
// We have a ConnectionManager, which tracks the state of all connections. It has a
// DashMap of connection IDs to connection state structures, which contain details
// about each connection, as well as a channel through which we can send packets to the
// connection.
//
// Connections implement Stream<Message>, with data that comes from one or more packets.
//
// Each connection shares access to the underlying socket. When a connection wants to
// read a packet, it stores a future to read the socket, and polls it. If a packet is
// received, we check the connection ID field. If it doesn't match the current
// connection's ID, then we route the packet through the connection manager, which sends
// the packet to the corresponding connection through a channel. When a connection is
// dropped, it removes its state entry from the DashMap.
//
// We can get connections using the listener by implementing Stream<Connection>. If the
// next packet is a SYN, initiate the handshake process by adding an entry to the
// connection manager's DashMap, then return Poll::Pending. The next time the listener
// is polled, we can advance the state of pending connections by modifying the entries in
// the connection manager's DashMap, and returning Poll::Pending if not yet ready.
//
// All connection states are held inside a single DashMap, where the values are of some
// type `ConnectionState`

pub struct UtpListener {
    socket: Arc<Mutex<UtpSocket>>,
    syn_packet_rx: UnboundedReceiver<(Packet, SocketAddr)>,
    connection_manager: Arc<ConnectionManager>,
    read_future: Option<LocalBoxFuture<'static, Result<(Packet, SocketAddr)>>>,
}

impl UtpListener {
    /// Creates a new UtpListener, which will be bound to the specified address.
    pub async fn bind(addr: impl ToSocketAddrs) -> Result<Self> {
        let (syn_packet_tx, syn_packet_rx) = unbounded_channel();
        let connection_manager = ConnectionManager::new(Default::default(), syn_packet_tx);
        Ok(UtpListener {
            socket: Arc::new(Mutex::new(UtpSocket::bind(addr).await?)),
            syn_packet_rx,
            connection_manager: Arc::new(connection_manager),
            read_future: None,
        })
    }
}

impl Stream for UtpListener {
    type Item = Result<Connection>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        // TODO: Confirm desired behavior. This is what the below logic currently does:
        //
        // - if there is a SYN packet in our buffered channel, extract the packet and address
        // - else if there is a pending future to read from the socket, poll it with `ready!`
        //   - if this gives us a result, extract the packet and address
        // - else, create a future to read from the socket and poll it with `ready!`
        //   - if this gives us a result, extract the packet and address
        // - we are now guaranteed to have a packet and an address.

        let result = if let Poll::Ready(Some((packet, addr))) = self.syn_packet_rx.poll_recv(cx) {
            Ok((packet, addr))
        } else if self.read_future.is_some() {
            let packet_and_addr = ready!(self.read_future.as_mut().unwrap().as_mut().poll(cx));
            // Remove the future if it finished
            self.read_future.take();
            packet_and_addr
        } else {
            let socket = Arc::clone(&self.socket);
            self.read_future = Some(Box::pin(async move {
                let mut socket = socket.lock().await;
                socket.recv_from().await
            }));
            let packet_and_addr = ready!(self.read_future.as_mut().unwrap().as_mut().poll(cx));
            // Remove the future if it finished
            self.read_future.take();
            packet_and_addr
        };

        match result {
            Ok((packet, addr)) => match packet.packet_type {
                PacketType::Syn => {
                    let (connection_tx, connection_rx) = unbounded_channel();
                    let new_state = ConnectionState::new(addr.clone(), false, connection_tx);
                    if self
                        .connection_manager
                        .set_state(packet.connection_id, new_state)
                    {
                        // TODO: Craft valid state packet to respond to SYN
                        let state_packet = Packet::new(
                            PacketType::State,
                            1,
                            packet.connection_id,
                            0,
                            0,
                            0,
                            0,
                            0,
                            vec![],
                            Bytes::new(),
                        );
                        let socket = Arc::clone(&self.socket);
                        return Poll::Ready(Some(Ok(Connection::new(
                            Arc::clone(&self.socket),
                            Arc::clone(&self.connection_manager),
                            connection_rx,
                            None,
                            Some(Box::pin(async move {
                                let mut socket = socket.lock().await;
                                socket.send_to(state_packet, addr).await
                            })),
                        ))));
                    } else {
                        // If we already have state for the connection ID, skip the packet
                        return Poll::Pending;
                    }
                }
                _ => todo!(), // TODO: Route packet through connection manager
            },
            Err(err) => return Poll::Ready(Some(Err(err))),
        }
    }
}
