use std::{fmt, net::SocketAddr, sync::Arc};

use bytes::BytesMut;
use crossbeam_queue::SegQueue;
use log::debug;
use tokio::net::{lookup_host, ToSocketAddrs};

use crate::{error::*, packet::Packet, socket::UtpSocket};

// TODO: Need to figure out a plan to deal with lost packets
#[allow(dead_code)]
pub struct UtpStream {
    socket: Arc<UtpSocket>,
    connection_id: u16,
    remote_addr: SocketAddr,
    // TODO: Track connection state
    outbound_packets: SegQueue<Packet>,
    sent_packets: SegQueue<Packet>,
    inbound_packets: SegQueue<Packet>,
    received_data: BytesMut,
}

impl UtpStream {
    pub(crate) fn new(
        socket: Arc<UtpSocket>,
        connection_id: u16,
        remote_addr: SocketAddr,
        outbound_packets: SegQueue<Packet>,
        sent_packets: SegQueue<Packet>,
        inbound_packets: SegQueue<Packet>,
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
        let connection_id = socket.register_connection(remote_addr).await?;
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
        // TODO: Add packet data to some kind of internal buffer
        Ok(())
    }

    // TODO: async fn write() to add to outgoing packet buffer
    // TODO: async fn flush() to flush outgoing packet buffer
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
