use std::{
    collections::VecDeque,
    fmt, io,
    net::SocketAddr,
    sync::{Arc, RwLock},
};

use bytes::{Bytes, BytesMut};
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
    ) -> Self {
        Self {
            socket,
            connection_id,
            remote_addr,
            outbound_packets,
            sent_packets,
            inbound_packets,
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

    #[allow(dead_code)]
    async fn send(&self, buffer: &[u8]) -> io::Result<usize> {
        let mut outbound_packets = self.outbound_packets.write().unwrap();
        buffer.chunks(1400).for_each(|chunk| {
            #[rustfmt::skip]
            // TODO: Use actual values for packet fields
            outbound_packets.push_back(Packet::new(PacketType::Data, 1, self.connection_id(), 0, 0, 0,
                                                   0, 0, vec![], Bytes::copy_from_slice(chunk)));
        });
        self.flush().await
    }

    #[allow(dead_code)]
    async fn flush(&self) -> io::Result<usize> {
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
