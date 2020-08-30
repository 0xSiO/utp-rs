use std::{
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use crossbeam_deque::Injector;
use dashmap::{DashMap, ElementGuard};
use futures_util::{
    future::{FutureExt, LocalBoxFuture},
    ready,
    stream::Stream,
};
use tokio::{
    net::ToSocketAddrs,
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        Mutex,
    },
};

use crate::{
    error::*,
    packet::{Packet, PacketType},
    UtpSocket,
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

pub struct ConnectionState {
    remote_addr: SocketAddr,
    established: bool,
}

impl ConnectionState {
    pub fn new(remote_addr: SocketAddr, established: bool) -> Self {
        Self {
            remote_addr,
            established,
        }
    }
}

pub struct Connection {
    socket: Arc<Mutex<UtpSocket>>,
    manager: Arc<ConnectionManager>,
    packet_rx: UnboundedReceiver<Packet>,
    // TODO: Double-check lifetimes of boxed futures
    read_future: Option<LocalBoxFuture<'static, Packet>>,
    write_future: Option<LocalBoxFuture<'static, Packet>>,
}

impl Stream for Connection {
    type Item = (); // some "message" type

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Option<Self::Item>> {
        todo!()
    }
}

pub struct ConnectionManager {
    connection_states: DashMap<u16, ConnectionState>,
    syn_packet_tx: UnboundedSender<(Packet, SocketAddr)>,
}

impl ConnectionManager {
    pub fn new(
        connection_states: DashMap<u16, ConnectionState>,
        syn_packet_tx: UnboundedSender<(Packet, SocketAddr)>,
    ) -> Self {
        Self {
            connection_states,
            syn_packet_tx,
        }
    }

    pub fn get_state(&self, id: u16) -> Option<ElementGuard<u16, ConnectionState>> {
        self.connection_states.get(&id)
    }

    pub fn set_state(&self, id: u16, state: ConnectionState) -> bool {
        if self.connection_states.contains_key(&id) {
            false
        } else {
            // TODO: This could cause a bug where state is mysteriously overridden
            //       If this is a problem, consider using Arc<Mutex<HashMap>>
            let success = self.connection_states.insert(id, state);
            // Make absolutely sure the state didn't already exist
            debug_assert!(success);
            true
        }
    }

    pub fn route(&self, packet: Packet) {
        todo!()
    }
}

pub struct UtpListener {
    socket: Arc<Mutex<UtpSocket>>,
    syn_packet_tx: UnboundedSender<(Packet, SocketAddr)>,
    syn_packet_rx: UnboundedReceiver<(Packet, SocketAddr)>,
    connection_manager: Arc<ConnectionManager>,
    read_future: Option<LocalBoxFuture<'static, Result<(Packet, SocketAddr)>>>,
    write_future: Option<LocalBoxFuture<'static, Result<usize>>>,
}

impl UtpListener {
    /// Creates a new UtpListener, which will be bound to the specified address.
    ///
    /// The returned listener is ready to begin listening for incoming messages.
    ///
    /// Binding with a port number of 0 will request that the OS assigns a port to this
    /// listener. The port allocated can be queried via the local_addr method.
    ///
    /// If addr yields multiple addresses, bind will be attempted with each of the
    /// addresses until one succeeds and returns the listener. If none of the addresses
    /// succeed in creating a listener, the error returned from the last attempt (the last
    /// address) is returned.
    pub async fn bind(addr: impl ToSocketAddrs) -> Result<Self> {
        let (syn_packet_tx, syn_packet_rx) = unbounded_channel();
        Ok(UtpListener {
            socket: Arc::new(Mutex::new(UtpSocket::bind(addr).await?)),
            syn_packet_tx: syn_packet_tx.clone(),
            syn_packet_rx,
            connection_manager: Arc::new(ConnectionManager::new(Default::default(), syn_packet_tx)),
            read_future: None,
            write_future: None,
        })
    }
}

impl Stream for UtpListener {
    type Item = Result<Connection>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        // TODO: Confirm desired behavior. This is what the below logic currently does:
        //
        // Check for writes:
        // - if we have a pending future to write to the socket, poll that
        //   - if this gives an Ok, continue to next section
        //   - else, return an error
        //
        // Check for reads:
        // - if there is a SYN packet in our buffered channel, extract the packet and address
        // - else if there is a pending future to read from the socket, poll it with `ready!`
        //   - if this gives us a result, extract the packet and address
        // - else, create a future to read from the socket and poll it with `ready!`
        //   - if this gives us a result, extract the packet and address
        //
        // At this point we are guaranteed to have a packet and an address.
        // - TODO: If we need to write a response to the socket, create a future to do so
        //         and return pending

        // TODO: This only lets us write one packet at a time. Maybe we want to queue writes
        if self.write_future.is_some() {
            if let Poll::Ready(result) = self.write_future.as_mut().unwrap().as_mut().poll(cx) {
                // Remove the finished future
                self.write_future.take();
                match result {
                    Ok(_) => {}
                    Err(err) => return Poll::Ready(Some(Err(err))),
                }
            }
        }

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
                    let new_state = ConnectionState::new(addr.clone(), false);
                    if self
                        .connection_manager
                        .set_state(packet.connection_id, new_state)
                    {
                        // TODO: Craft state packet to respond to SYN
                        // let state_packet = Packet::new();
                        // let socket = Arc::clone(&self.socket);
                        // self.write_future = Some(Box::pin(async move {
                        //     let mut socket = socket.lock().await;
                        //     socket.send_to(state_packet, addr).await
                        // }));
                        return Poll::Pending;
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
