use std::{
    collections::{HashMap, VecDeque},
    fmt, io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::{Bytes, BytesMut};
use futures_util::{ready, stream::TryStreamExt};
use log::debug;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{lookup_host, ToSocketAddrs},
};

use crate::{
    error::*,
    packet::{Extension, Packet, PacketType},
    socket::UtpSocket,
};

pub(crate) const MAX_DATA_SEGMENT_SIZE: usize =
    crate::socket::MAX_DATAGRAM_SIZE - crate::packet::PACKET_HEADER_LEN;

#[allow(dead_code)]
pub struct UtpStream {
    socket: Arc<UtpSocket>,
    connection_id_recv: u16,
    connection_id_send: u16,
    remote_addr: SocketAddr,
    // TODO: Track connection state (connected, shutdown, etc.)
    seq_number: u16,
    ack_number: u16,
    inbound_data: HashMap<u16, Bytes>,
    received_data: BytesMut,
    outbound_data: VecDeque<Bytes>,
    inbound_acks: HashMap<u16, Packet>,
}

impl UtpStream {
    pub(crate) fn new(
        socket: Arc<UtpSocket>,
        connection_id_recv: u16,
        connection_id_send: u16,
        remote_addr: SocketAddr,
        seq_number: u16,
        ack_number: u16,
        inbound_data: HashMap<u16, Bytes>,
        received_data: BytesMut,
        outbound_data: VecDeque<Bytes>,
        inbound_acks: HashMap<u16, Packet>,
    ) -> Self {
        Self {
            socket,
            connection_id_recv,
            connection_id_send,
            remote_addr,
            seq_number,
            ack_number,
            inbound_data,
            received_data,
            outbound_data,
            inbound_acks,
        }
    }

    /// Create a new [`UtpStream`] using the given socket, and attempt to connect the [`UtpStream`]
    /// to the provided address.
    pub async fn connect(socket: Arc<UtpSocket>, remote_addr: impl ToSocketAddrs) -> Result<Self> {
        let remote_addr = lookup_host(remote_addr)
            .await?
            .next()
            .ok_or(Error::MissingAddress)?;

        let connection_id_recv = socket.register_connection(remote_addr);
        let connection_id_send = connection_id_recv.wrapping_add(1);
        let seq_number = 1;
        // Just set ack_number to 0, this shouldn't be read by the remote socket anyway
        let ack_number = 0;
        #[rustfmt::skip]
        let syn = Packet::new(PacketType::Syn, 1, connection_id_recv, 0, 0, 0, seq_number,
                              ack_number, vec![], Bytes::new());
        let seq_number = seq_number.wrapping_add(1);
        socket.send_to(syn, remote_addr).await?;

        // state: SYN sent

        // TODO: Handle this more gracefully
        let response_packet = socket
            .packets(connection_id_recv, remote_addr)
            .try_next()
            .await?
            .unwrap();

        match response_packet.packet_type {
            PacketType::State => {
                // state: connected
                Ok(Self::new(
                    socket,
                    connection_id_recv,
                    connection_id_send,
                    remote_addr,
                    seq_number,
                    response_packet.seq_number,
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    Default::default(),
                ))
            }
            // TODO: other packet types not allowed
            _ => todo!(),
        }
    }

    /// Return the connection ID used to receive packets for this connection. This ID is the one
    /// registered in the [`UtpSocket`] internal routing table.
    pub fn connection_id_recv(&self) -> u16 {
        self.connection_id_recv
    }

    /// Return the connection ID used to send packets to the remote socket.
    pub fn connection_id_send(&self) -> u16 {
        self.connection_id_send
    }

    /// Return the local [`SocketAddr`] to which the socket for this connection is bound.
    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr()
    }

    /// Return the remote [`SocketAddr`] to which this connection is sending packets.
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    fn poll_send_packet(
        &self,
        cx: &mut Context<'_>,
        packet_type: PacketType,
        seq_number: u16,
        ack_number: u16,
        extensions: Vec<Extension>,
        data: Bytes,
    ) -> Poll<io::Result<()>> {
        let now = SystemTime::now();
        let total_microseconds = now.duration_since(UNIX_EPOCH).unwrap().as_micros();
        // We really only need the last 32 bits of the current time. This is meant to measure
        // delays between sender and receiver, and u32::MAX microseconds is about 72 minutes,
        // which should be more than enough to measure a one-way transmission delay.
        let truncated_microseconds = (total_microseconds & u32::MAX as u128) as u32;

        // TODO: Fill out the rest of the packet fields
        #[rustfmt::skip]
        let packet = Packet::new(packet_type, 1, self.connection_id_send(), truncated_microseconds,
                                 0, 0, seq_number, ack_number, extensions, data);
        let _bytes_sent = ready!(self.socket.poll_send_to(cx, packet, self.remote_addr()))?;
        // TODO: Add bytes sent to current window
        Poll::Ready(Ok(()))
    }

    fn handle_packet(&mut self, packet: Packet) {
        debug!("Handling inbound packet: {:?}", packet);
        match packet.packet_type {
            PacketType::Data => {
                // Queue up the data packet to be processed during poll_read

                // TODO: What if we already have this packet?
                self.inbound_data.insert(packet.seq_number, packet.data);
            }
            PacketType::State => {
                // Queue up the ACK to be processed during poll_flush

                // TODO: What if we already have this packet?
                self.inbound_acks.insert(packet.ack_number, packet);
            }
            _ => {
                // TODO: Respond to other packet types
                todo!()
            }
        }
    }

    fn poll_send_ack(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_send_packet(
            cx,
            PacketType::State,
            self.seq_number,
            self.ack_number,
            vec![],
            Bytes::new(),
        )
    }

    fn poll_send_data(&self, cx: &mut Context<'_>, data: Bytes) -> Poll<io::Result<()>> {
        self.poll_send_packet(
            cx,
            PacketType::Data,
            self.seq_number,
            self.ack_number,
            vec![],
            data,
        )
    }
}

impl fmt::Debug for UtpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "UtpStream {{ id: {}, local_addr: {}, remote_addr: {} }}",
            self.connection_id_recv,
            self.socket.local_addr(),
            self.remote_addr
        ))
    }
}

// TODO: Do some more research on how packets should actually be sent/received :)

impl AsyncRead for UtpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        todo!()
    }
}

impl AsyncWrite for UtpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // TODO: Don't copy each chunk, use Bytes::split_to to split up the data
        self.outbound_data.extend(
            buf.chunks(MAX_DATA_SEGMENT_SIZE)
                .map(Bytes::copy_from_slice),
        );
        Poll::Ready(Ok(buf.len()))
    }

    // TODO: Any extra required logic to deal with duplicate ACKs and lost packets
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        todo!()
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_flush(cx))?;
        // TODO: We could set a shutdown flag on UtpStream and return Ok(0) for any future calls to
        //       poll_write, effectively preventing any more packets from being sent
        todo!()
    }
}
