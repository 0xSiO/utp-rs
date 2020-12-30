use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, RwLock},
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use futures_util::{future::LocalBoxFuture, io::AsyncWrite, ready};
use log::debug;
use tokio::net::{lookup_host, ToSocketAddrs};

use crate::{
    error::*,
    packet::{Packet, PacketType},
    socket::UtpSocket,
};

// TODO: Need to figure out a plan to deal with lost packets
#[allow(dead_code)]
pub struct UtpStream {
    socket: Arc<UtpSocket>,
    connection_id: u16,
    remote_addr: SocketAddr,
    // TODO: Track connection state
    outbound_packets: RwLock<VecDeque<Packet>>,
    sent_packets: RwLock<VecDeque<Packet>>,
    inbound_packets: RwLock<VecDeque<Packet>>,
    received_data: BytesMut,
    write_future: Option<LocalBoxFuture<'static, io::Result<usize>>>,
}

impl UtpStream {
    pub(crate) fn new(
        socket: Arc<UtpSocket>,
        connection_id: u16,
        remote_addr: SocketAddr,
        outbound_packets: RwLock<VecDeque<Packet>>,
        sent_packets: RwLock<VecDeque<Packet>>,
        inbound_packets: RwLock<VecDeque<Packet>>,
        received_data: BytesMut,
        write_future: Option<LocalBoxFuture<'static, io::Result<usize>>>,
    ) -> Self {
        Self {
            socket,
            connection_id,
            remote_addr,
            outbound_packets,
            sent_packets,
            inbound_packets,
            received_data,
            write_future,
        }
    }

    pub async fn connect(socket: Arc<UtpSocket>, remote_addr: impl ToSocketAddrs) -> Result<Self> {
        let remote_addr = lookup_host(remote_addr)
            .await?
            .next()
            .ok_or_else(|| Error::MissingAddress)?;
        let connection_id = socket.register_connection(remote_addr)?;
        Ok(Self::new(
            socket,
            connection_id,
            remote_addr,
            // TODO: Queue up a SYN to send
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        ))
    }

    pub fn connection_id(&self) -> u16 {
        self.connection_id
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr()
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    pub async fn recv(&self) -> Result<()> {
        let packet = self
            .socket
            .get_packet(self.connection_id, self.remote_addr)
            .await?;
        debug!(
            "connection {} received {:?} from {}",
            self.connection_id, packet.packet_type, self.remote_addr
        );
        // TODO: Add packet data to some kind of internal buffer, but make sure it's in order
        Ok(())
    }

    async fn write_outbound_packets(&self) -> io::Result<usize> {
        let mut bytes_written: usize = 0;
        while let Some(packet) = self.outbound_packets.write().unwrap().pop_front() {
            match self.socket.send_to(packet.clone(), self.remote_addr).await {
                Ok(num_bytes) => {
                    bytes_written += num_bytes;
                    self.sent_packets.write().unwrap().push_back(packet);
                }
                err => {
                    // Re-queue the packet to try sending again
                    self.outbound_packets.write().unwrap().push_front(packet);
                    return err;
                }
            }
        }
        Ok(bytes_written)
    }

    fn poll_write_outbound_packets(&mut self, cx: &mut Context) -> Poll<io::Result<usize>> {
        let bytes_written = ready!(self.write_future.as_mut().unwrap().as_mut().poll(cx));
        // Remove the future if it finished
        self.write_future.take();
        Poll::Ready(bytes_written)
    }
}

impl fmt::Debug for UtpStream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_fmt(format_args!(
            "UtpStream {{ id: {}, local_addr: {}, remote_addr: {} }}",
            self.connection_id,
            self.socket.local_addr(),
            self.remote_addr
        ))
    }
}

impl AsyncWrite for UtpStream {
    // Split given buffer into packets, add packets to outgoing packet buffer, then poll a stored
    // future
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // TODO: Make sure any initialization is out of the way first
        #[rustfmt::skip]
        let packets = buf.chunks(1400).map(|chunk| {
            // TODO: Fill out rest of packet fields later
            Packet::new(PacketType::Data, 1, self.connection_id(), 0, 0, 0, 0, 0, vec![],
                Bytes::copy_from_slice(chunk))
        });

        {
            let mut outbound_packets = self.outbound_packets.write().unwrap();
            for packet in packets {
                outbound_packets.push_back(packet);
            }
        }

        if self.write_future.is_some() {
            self.poll_write_outbound_packets(cx)
        } else {
            self.write_future = Some(Box::pin(async move { self.write_outbound_packets().await }));
            self.poll_write_outbound_packets(cx)
        }
    }

    // Poll stored future if available, if not, create and poll
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        todo!()
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}
