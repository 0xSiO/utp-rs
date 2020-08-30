use std::{
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures_util::{future::LocalBoxFuture, stream::Stream};
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    Mutex,
};

use crate::{connection_manager::ConnectionManager, error::*, packet::Packet, UtpSocket};

pub struct ConnectionState {
    remote_addr: SocketAddr,
    established: bool,
    incoming_packets: UnboundedSender<Packet>,
}

impl ConnectionState {
    pub fn new(
        remote_addr: SocketAddr,
        established: bool,
        incoming_packets: UnboundedSender<Packet>,
    ) -> Self {
        Self {
            remote_addr,
            established,
            incoming_packets,
        }
    }
}

pub struct Connection {
    socket: Arc<Mutex<UtpSocket>>,
    manager: Arc<ConnectionManager>,
    packet_rx: UnboundedReceiver<Packet>,
    // TODO: Double-check lifetimes of boxed futures
    read_future: Option<LocalBoxFuture<'static, Result<(Packet, SocketAddr)>>>,
    write_future: Option<LocalBoxFuture<'static, Result<usize>>>,
}

impl Connection {
    pub fn new(
        socket: Arc<Mutex<UtpSocket>>,
        manager: Arc<ConnectionManager>,
        packet_rx: UnboundedReceiver<Packet>,
        read_future: Option<LocalBoxFuture<'static, Result<(Packet, SocketAddr)>>>,
        write_future: Option<LocalBoxFuture<'static, Result<usize>>>,
    ) -> Self {
        Self {
            socket,
            manager,
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
