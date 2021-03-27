use std::{
    collections::VecDeque,
    fmt, io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, RwLock},
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use log::debug;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{lookup_host, ToSocketAddrs},
};

use crate::{
    error::*,
    packet::{Packet, PacketType},
    socket::UtpSocket,
};

pub(crate) const MAX_DATA_SEGMENT_SIZE: usize =
    crate::socket::MAX_DATAGRAM_SIZE - crate::packet::PACKET_HEADER_LEN;

// TODO: Need to figure out a plan to deal with lost packets
#[allow(dead_code)]
pub struct UtpStream {
    socket: Arc<UtpSocket>,
    connection_id: u16,
    remote_addr: SocketAddr,
    // TODO: Track connection state
    outbound_packets: Arc<RwLock<VecDeque<Packet>>>,
    sent_packets: Arc<RwLock<VecDeque<Packet>>>,
    received_packets: Arc<RwLock<VecDeque<Packet>>>,
    received_data: BytesMut,
}

impl UtpStream {
    pub(crate) fn new(
        socket: Arc<UtpSocket>,
        connection_id: u16,
        remote_addr: SocketAddr,
        outbound_packets: Arc<RwLock<VecDeque<Packet>>>,
        sent_packets: Arc<RwLock<VecDeque<Packet>>>,
        received_packets: Arc<RwLock<VecDeque<Packet>>>,
        received_data: BytesMut,
    ) -> Self {
        Self {
            socket,
            connection_id,
            remote_addr,
            outbound_packets,
            sent_packets,
            received_packets,
            received_data,
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

    pub async fn recv(&mut self) -> Result<()> {
        let packet = self
            .socket
            .get_packet(self.connection_id, self.remote_addr)
            .await?;
        debug!(
            "connection {} received {:?} from {}",
            self.connection_id, packet.packet_type, self.remote_addr
        );

        // Reply with State if we received a Data
        match packet.packet_type {
            PacketType::Data => {
                // TODO: Use actual values for packet fields
                #[rustfmt::skip]
                let ack = Packet::new(PacketType::State, 1, self.connection_id(), 0, 0, 0, 0,
                                      packet.seq_number, vec![], Bytes::new());
                self.outbound_packets.write().unwrap().push_back(ack);
                // TODO: This will send all packets waiting in the outbound buffer. Is this the
                //       behavior we want?
                self.flush().await?;
            }
            _ => {}
        }

        self.received_packets.write().unwrap().push_back(packet);

        Ok(())
    }

    fn poll_read_priv(
        &mut self,
        _cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        todo!()
    }

    fn poll_write_priv(&mut self, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let mut outbound_packets = self.outbound_packets.write().unwrap();
        // TODO: Don't copy each chunk, use Bytes::split_to to get the bytes for each packet
        buf.chunks(MAX_DATA_SEGMENT_SIZE).for_each(|chunk| {
            // TODO: Use actual values for packet fields
            #[rustfmt::skip]
            let packet = Packet::new(PacketType::Data, 1, self.connection_id(), 0, 0, 0, 0, 0,
                                     vec![], Bytes::copy_from_slice(chunk));
            outbound_packets.push_back(packet);
        });
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush_priv(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while let Some(packet) = self.outbound_packets.write().unwrap().pop_front() {
            match self
                .socket
                .poll_send_to(cx, packet.clone(), self.remote_addr)
            {
                Poll::Pending => {
                    self.outbound_packets.write().unwrap().push_front(packet);
                    return Poll::Pending;
                }
                // TODO: Limit number of retries?
                Poll::Ready(Err(err)) if err.kind() == io::ErrorKind::Interrupted => {
                    self.outbound_packets.write().unwrap().push_front(packet);
                    continue;
                }
                Poll::Ready(Err(err)) => {
                    self.outbound_packets.write().unwrap().push_front(packet);
                    return Poll::Ready(Err(err));
                }
                Poll::Ready(Ok(_)) => {
                    self.sent_packets.write().unwrap().push_back(packet);
                }
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl fmt::Debug for UtpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "UtpStream {{ id: {}, local_addr: {}, remote_addr: {} }}",
            self.connection_id,
            self.socket.local_addr(),
            self.remote_addr
        ))
    }
}

impl AsyncRead for UtpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.poll_read_priv(cx, buf)
    }
}

impl AsyncWrite for UtpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_priv(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush_priv(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // TODO: We could set a shutdown flag on UtpStream and return Ok(0) for any future calls to
        //       poll_write, effectively preventing any more packets from being sent
        self.poll_flush(cx)
    }
}
