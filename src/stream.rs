use std::{
    collections::VecDeque,
    fmt, io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex, RwLock},
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use futures_util::{future::BoxFuture, io::AsyncWrite, ready};
use log::debug;
use tokio::net::{lookup_host, ToSocketAddrs};

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
    // TODO: Don't use outbound packets, use outbound byte slices (packets contain timing
    //       information which will be stale when it's time to actually send the packets)
    outbound_packets: Arc<RwLock<VecDeque<Packet>>>,
    sent_packets: Arc<RwLock<VecDeque<Packet>>>,
    inbound_packets: Arc<RwLock<VecDeque<Packet>>>,
    received_data: BytesMut,
    // Technically 'static to avoid self-referential fields, but should never outlive the stream.
    // The future is wrapped in a Mutex to make the whole stream Sync.
    flush_future: Option<Mutex<BoxFuture<'static, io::Result<()>>>>,
}

impl UtpStream {
    pub(crate) fn new(
        socket: Arc<UtpSocket>,
        connection_id: u16,
        remote_addr: SocketAddr,
        outbound_packets: Arc<RwLock<VecDeque<Packet>>>,
        sent_packets: Arc<RwLock<VecDeque<Packet>>>,
        inbound_packets: Arc<RwLock<VecDeque<Packet>>>,
        received_data: BytesMut,
        flush_future: Option<Mutex<BoxFuture<'static, io::Result<()>>>>,
    ) -> Self {
        Self {
            socket,
            connection_id,
            remote_addr,
            outbound_packets,
            sent_packets,
            inbound_packets,
            received_data,
            flush_future,
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
        // TODO: Make sure data is added to internal buffer in the correct packet order, right now
        //       it could be completely out of order depending on how the packets arrive
        Ok(())
    }

    async fn flush(
        socket: Arc<UtpSocket>,
        remote_addr: SocketAddr,
        outbound_packets: Arc<RwLock<VecDeque<Packet>>>,
        sent_packets: Arc<RwLock<VecDeque<Packet>>>,
    ) -> io::Result<()> {
        loop {
            let maybe_packet = outbound_packets.write().unwrap().pop_front();
            if let Some(packet) = maybe_packet {
                match socket.send_to(packet.clone(), remote_addr).await {
                    Ok(_) => {
                        sent_packets.write().unwrap().push_back(packet);
                    }
                    Err(err) => {
                        // Re-queue the packet to try sending again
                        outbound_packets.write().unwrap().push_front(packet);
                        return Err(err);
                    }
                }
            } else {
                break;
            }
        }
        Ok(())
    }

    fn init_flush_future(&mut self) {
        let socket = Arc::clone(&self.socket);
        let remote_addr = self.remote_addr();
        let outbound_packets = Arc::clone(&self.outbound_packets);
        let sent_packets = Arc::clone(&self.sent_packets);
        self.flush_future = Some(Mutex::new(Box::pin(async move {
            Self::flush(socket, remote_addr, outbound_packets, sent_packets).await
        })));
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
    fn poll_write(self: Pin<&mut Self>, _cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        let mut outbound_packets = self.outbound_packets.write().unwrap();
        // TODO: Don't copy each chunk, use Bytes::split_to to get the bytes for each packet
        buf.chunks(MAX_DATA_SEGMENT_SIZE).for_each(|chunk| {
            #[rustfmt::skip]
            // TODO: Use actual values for packet fields
            outbound_packets.push_back(Packet::new(PacketType::Data, 1, self.connection_id(), 0, 0, 0,
                                                   0, 0, vec![], Bytes::copy_from_slice(chunk)));
        });
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        if self.flush_future.is_none() {
            self.init_flush_future();
        }

        let result = ready!({
            // TODO: Should we use try_lock here?
            let mut future = self.flush_future.as_mut().unwrap().lock().unwrap();
            future.as_mut().poll(cx)
        });
        // Remove the future if it finished
        self.flush_future.take();
        Poll::Ready(result)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        // TODO: We could set a shutdown flag on UtpStream and return Ok(0) for any future calls to
        //       poll_write, effectively preventing any more packets from being sent
        self.poll_flush(cx)
    }
}
