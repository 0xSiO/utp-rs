use std::{
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures_util::{future::LocalBoxFuture, stream::Stream};
use tokio::sync::{mpsc::UnboundedReceiver, Mutex};

use crate::{error::*, packet::Packet, router::Router, socket::UtpSocket};

pub struct Connection {
    socket: Arc<Mutex<UtpSocket>>,
    remote_addr: SocketAddr,
    established: bool,
    router: Arc<Router>,
    packet_rx: UnboundedReceiver<(Packet, SocketAddr)>,
    // TODO: Double-check lifetimes of boxed futures
    read_future: Option<LocalBoxFuture<'static, Result<(Packet, SocketAddr)>>>,
    write_future: Option<LocalBoxFuture<'static, Result<usize>>>,
}

impl Connection {
    pub fn new(
        socket: Arc<Mutex<UtpSocket>>,
        remote_addr: SocketAddr,
        established: bool,
        router: Arc<Router>,
        packet_rx: UnboundedReceiver<(Packet, SocketAddr)>,
        read_future: Option<LocalBoxFuture<'static, Result<(Packet, SocketAddr)>>>,
        write_future: Option<LocalBoxFuture<'static, Result<usize>>>,
    ) -> Self {
        Self {
            socket,
            remote_addr,
            established,
            router,
            packet_rx,
            read_future,
            write_future,
        }
    }
}

impl Stream for Connection {
    type Item = (); // some "message" type

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Option<Self::Item>> {
        todo!()
    }
}
