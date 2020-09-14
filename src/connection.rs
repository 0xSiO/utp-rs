use std::{
    fmt,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use flume::{unbounded, Receiver};
use futures_util::{future::BoxFuture, ready, stream::Stream};
use log::debug;

use crate::{error::*, packet::Packet, router::Router, socket::UtpSocket};

// TODO: Need to figure out a plan to deal with lost packets: one idea is to have a queue
// of unacked packets, pass a reference into the write future, and access the queue from
// the future... something like that
pub struct Connection {
    socket: Arc<UtpSocket>,
    connection_id: u16,
    remote_addr: SocketAddr,
    router: Arc<Router>,
    packet_rx: Receiver<(Packet, SocketAddr)>,
    read_future: Option<BoxFuture<'static, Result<(Packet, SocketAddr)>>>,
    write_future: Option<BoxFuture<'static, Result<usize>>>,
    route_future: Option<BoxFuture<'static, ()>>,
}

impl Connection {
    pub fn new(
        socket: Arc<UtpSocket>,
        connection_id: u16,
        remote_addr: SocketAddr,
        router: Arc<Router>,
        packet_rx: Receiver<(Packet, SocketAddr)>,
        read_future: Option<BoxFuture<'static, Result<(Packet, SocketAddr)>>>,
        write_future: Option<BoxFuture<'static, Result<usize>>>,
        route_future: Option<BoxFuture<'static, ()>>,
    ) -> Self {
        Self {
            socket,
            connection_id,
            remote_addr,
            router,
            packet_rx,
            read_future,
            write_future,
            route_future,
        }
    }

    pub fn connection_id(&self) -> u16 {
        self.connection_id
    }

    pub async fn generate(
        socket: Arc<UtpSocket>,
        router: Arc<Router>,
        remote_addr: SocketAddr,
    ) -> Result<Self> {
        let (packet_tx, packet_rx) = unbounded();

        Ok(Self::new(
            socket,
            router.register_channel(packet_tx).await?,
            remote_addr,
            router,
            packet_rx,
            None,
            None, // TODO: Write SYN packet to remote socket
            None,
        ))
    }

    fn poll_read(&mut self, cx: &mut Context) -> Poll<Result<(Packet, SocketAddr)>> {
        let packet_and_addr = ready!(self.read_future.as_mut().unwrap().as_mut().poll(cx));
        // Remove the future if it finished
        self.read_future.take();
        Poll::Ready(packet_and_addr)
    }

    fn poll_write(&mut self, cx: &mut Context) -> Poll<Result<usize>> {
        let bytes_written = ready!(self.write_future.as_mut().unwrap().as_mut().poll(cx));
        // Remove the future if it finished
        self.write_future.take();
        Poll::Ready(bytes_written)
    }

    fn poll_route(&mut self, cx: &mut Context) -> Poll<()> {
        ready!(self.route_future.as_mut().unwrap().as_mut().poll(cx));
        // Remove the future if it finished
        self.route_future.take();
        Poll::Ready(())
    }
}

impl Stream for Connection {
    type Item = Result<()>; // TODO: Add some "message" type

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        // Finish routing packets to their intended connections. We do not want to yield
        // if we're still holding onto unrouted packets, since if someone decides to
        // stop streaming us, the packet won't ever get routed!
        if self.route_future.is_some() {
            // No pending reads or writes.
            assert!(self.read_future.is_none());
            assert!(self.write_future.is_none());

            ready!(self.poll_route(cx))
        }

        if self.write_future.is_some() {
            // No pending reads or routes.
            assert!(self.read_future.is_none());
            assert!(self.route_future.is_none());

            // TODO: Handle this result in case it failed
            let _result = ready!(self.poll_write(cx));
        }

        // If we're in the middle of reading from the socket, finish doing that so other
        // connections can use the socket. Else, try reading incoming packets from our
        // channel.
        let mut packet_and_addr_opt = if self.read_future.is_some() {
            Some(ready!(self.poll_read(cx)))
        } else if let Ok(packet_and_addr) = self.packet_rx.try_recv() {
            Some(Ok(packet_and_addr))
        } else {
            None
        };

        // No pending futures.
        assert!(self.route_future.is_none());
        assert!(self.write_future.is_none());
        assert!(self.read_future.is_none());

        // A loop is used here in case we end up with this situation:
        //   No packet was received up until this point, so a read to the socket begins.
        //   If the first poll to the pending read gives us a packet, immediately
        //   process the packet by jumping to the beginning of the loop.
        //
        // Other branches of the match will return without continuing the loop, so this
        // shouldn't block.
        loop {
            match packet_and_addr_opt {
                Some(Ok((packet, addr))) => {
                    if packet.connection_id != self.connection_id {
                        // This packet isn't meant for us
                        debug!("{} routing unknown packet", self.socket.local_addr());
                        let router = Arc::clone(&self.router);
                        self.route_future =
                            Some(Box::pin(async move { router.route(packet, addr).await }));
                        ready!(self.poll_route(cx));
                        return Poll::Pending;
                    }

                    if self.remote_addr != addr {
                        // Somehow we got this packet from an unfamiliar address
                        // TODO: Log this event and drop the packet?
                    }

                    // TODO: Process the packet
                    return Poll::Ready(Some(Ok(())));
                }
                Some(Err(err)) => return Poll::Ready(Some(Err(err))),
                None => {
                    // We ended up with no packet, so as a last resort we start to read a
                    // packet from the socket.
                    let socket = Arc::clone(&self.socket);
                    self.read_future = Some(Box::pin(async move { socket.recv_from().await }));
                    packet_and_addr_opt.replace(ready!(self.poll_read(cx)));
                    // Got a packet! Jump to beginning of the loop to process it
                    continue;
                }
            }
        }
    }
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_fmt(format_args!(
            "Connection {{ id: {}, addr: {} }}",
            self.connection_id, self.remote_addr
        ))
    }
}
