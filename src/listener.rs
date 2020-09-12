use std::{
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use flume::{unbounded, Receiver};
use futures_util::{future::BoxFuture, ready, stream::Stream};
use tokio::net::ToSocketAddrs;

use crate::{
    connection::Connection,
    error::*,
    packet::{Packet, PacketType},
    router::Router,
    socket::UtpSocket,
};

pub struct UtpListener {
    socket: Arc<UtpSocket>,
    syn_packet_rx: Receiver<(Packet, SocketAddr)>,
    router: Arc<Router>,
    read_future: Option<BoxFuture<'static, Result<(Packet, SocketAddr)>>>,
    route_future: Option<BoxFuture<'static, ()>>,
    accept_future: Option<BoxFuture<'static, Option<Connection>>>,
}

impl UtpListener {
    pub fn new(
        socket: Arc<UtpSocket>,
        syn_packet_rx: Receiver<(Packet, SocketAddr)>,
        router: Arc<Router>,
        read_future: Option<BoxFuture<'static, Result<(Packet, SocketAddr)>>>,
        route_future: Option<BoxFuture<'static, ()>>,
        accept_future: Option<BoxFuture<'static, Option<Connection>>>,
    ) -> Self {
        Self {
            socket,
            syn_packet_rx,
            router,
            read_future,
            route_future,
            accept_future,
        }
    }

    /// Creates a new UtpListener, which will be bound to the specified address.
    pub async fn bind(addr: impl ToSocketAddrs) -> Result<Self> {
        let (syn_packet_tx, syn_packet_rx) = unbounded();
        let router = Router::new(Default::default(), Some(syn_packet_tx));
        Ok(Self::new(
            Arc::new(UtpSocket::bind(addr).await?),
            syn_packet_rx,
            Arc::new(router),
            None,
            None,
            None,
        ))
    }

    fn poll_read(&mut self, cx: &mut Context) -> Poll<Result<(Packet, SocketAddr)>> {
        let packet_and_addr = ready!(self.read_future.as_mut().unwrap().as_mut().poll(cx));
        // Remove the future if it finished
        self.read_future.take();
        Poll::Ready(packet_and_addr)
    }

    fn poll_route(&mut self, cx: &mut Context) -> Poll<()> {
        ready!(self.route_future.as_mut().unwrap().as_mut().poll(cx));
        // Remove the future if it finished
        self.route_future.take();
        Poll::Ready(())
    }

    // TODO: Should return type be a Result? We'd need to define an error like
    // 'ConnectionExists' when adding a channel to the router fails
    fn poll_accept(&mut self, cx: &mut Context) -> Poll<Option<Connection>> {
        let connection = ready!(self.accept_future.as_mut().unwrap().as_mut().poll(cx));
        // Remove the future if it finished
        self.accept_future.take();
        Poll::Ready(connection)
    }
}

impl Stream for UtpListener {
    type Item = Result<Connection>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        // Finish routing packets to their intended connections. We do not want to yield
        // any connections if we're still holding onto unrouted packets, since if someone
        // decides to stop streaming us, the packet won't ever get routed!
        if self.route_future.is_some() {
            // No pending reads or accepts.
            assert!(self.read_future.is_none());
            assert!(self.accept_future.is_none());

            ready!(self.poll_route(cx))
        }

        // Finish accepting any connections and yield if successful.
        if self.accept_future.is_some() {
            // We must have received a packet, so there are no pending reads now.
            // This is important because we do not want to be left holding a lock on
            // the socket with the possibility that no one will keep streaming us.
            assert!(self.read_future.is_none());
            // Similarly, we don't want to be left holding a lock on the router.
            assert!(self.route_future.is_none());

            if let Some(conn) = ready!(self.poll_accept(cx)) {
                return Poll::Ready(Some(Ok(conn)));
            }
        }

        // If we're in the middle of reading from the socket, finish doing that so other
        // connections can use the socket. Else, try reading incoming SYN packets from
        // our channel.
        let packet_and_addr_opt = if self.read_future.is_some() {
            Some(ready!(self.poll_read(cx)))
        } else if let Ok(packet_and_addr) = self.syn_packet_rx.try_recv() {
            Some(Ok(packet_and_addr))
        } else {
            None
        };

        // No pending futures.
        assert!(self.route_future.is_none());
        assert!(self.accept_future.is_none());
        assert!(self.read_future.is_none());

        match packet_and_addr_opt {
            Some(Ok((packet, addr))) => match packet.packet_type {
                PacketType::Syn => {
                    let router = Arc::clone(&self.router);
                    let socket = Arc::clone(&self.socket);
                    self.accept_future = Some(Box::pin(async move {
                        let (connection_tx, connection_rx) = unbounded();
                        if router
                            .set_channel(packet.connection_id, connection_tx)
                            .await
                        {
                            // TODO: Craft valid state packet to respond to SYN
                            #[rustfmt::skip]
                            let state_packet = Packet::new(
                                PacketType::State, 1, packet.connection_id,
                                0, 0, 0, 0, 0, vec![], Bytes::new(),
                            );
                            Some(Connection::new(
                                Arc::clone(&socket),
                                packet.connection_id,
                                addr,
                                Arc::clone(&router),
                                connection_rx,
                                None,
                                Some(Box::pin(
                                    async move { socket.send_to(state_packet, addr).await },
                                )),
                                None,
                            ))
                        } else {
                            None
                        }
                    }));

                    match ready!(self.poll_accept(cx)) {
                        Some(conn) => return Poll::Ready(Some(Ok(conn))),
                        None => return Poll::Pending,
                    }
                }
                _ => {
                    // This packet isn't meant for us
                    let router = Arc::clone(&self.router);
                    self.route_future =
                        Some(Box::pin(async move { router.route(packet, addr).await }));
                    ready!(self.poll_route(cx));
                    return Poll::Pending;
                }
            },
            Some(Err(err)) => return Poll::Ready(Some(Err(err))),
            None => {
                // We ended up with no packet, so as a last resort we start to read a
                // packet from the socket.
                let socket = Arc::clone(&self.socket);
                self.read_future = Some(Box::pin(async move { socket.recv_from().await }));
                // TODO: Since we immediately yield here without polling the socket,
                // are we adding unnecessary delay?
                return Poll::Pending;
            }
        }
    }
}
