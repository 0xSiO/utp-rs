use std::{
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures_core::{future::LocalBoxFuture, ready, stream::Stream};
use tokio::{
    net::ToSocketAddrs,
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver},
        Mutex,
    },
};

use crate::{
    connection::Connection,
    error::*,
    packet::{Packet, PacketType},
    router::Router,
    socket::UtpSocket,
};

pub struct UtpListener {
    socket: Arc<Mutex<UtpSocket>>,
    syn_packet_rx: UnboundedReceiver<(Packet, SocketAddr)>,
    router: Arc<Router>,
    read_future: Option<LocalBoxFuture<'static, Result<(Packet, SocketAddr)>>>,
}

impl UtpListener {
    /// Creates a new UtpListener, which will be bound to the specified address.
    pub async fn bind(addr: impl ToSocketAddrs) -> Result<Self> {
        let (syn_packet_tx, syn_packet_rx) = unbounded_channel();
        let router = Router::new(Default::default(), syn_packet_tx);
        Ok(UtpListener {
            socket: Arc::new(Mutex::new(UtpSocket::bind(addr).await?)),
            syn_packet_rx,
            router: Arc::new(router),
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
                    if self.router.set_channel(packet.connection_id, connection_tx) {
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
                            packet.connection_id,
                            addr,
                            Arc::clone(&self.router),
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
                _ => {
                    self.router.route(packet, addr);
                    return Poll::Pending;
                }
            },
            Err(err) => return Poll::Ready(Some(Err(err))),
        }
    }
}
