use std::{
    collections::{HashMap, VecDeque},
    fmt, io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use futures_util::{ready, stream::TryStreamExt};
use log::debug;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{lookup_host, ToSocketAddrs},
};

use crate::{
    congestion::CongestionController,
    error::*,
    packet::{Extension, Packet, PacketType},
    socket::UtpSocket,
    time::current_micros,
};

pub(crate) const MAX_DATA_SEGMENT_SIZE: usize =
    crate::socket::MAX_DATAGRAM_SIZE - crate::packet::PACKET_HEADER_LEN;

// TODO: Track connection state (connected, shutdown, etc.)
#[allow(dead_code)]
pub struct UtpStream {
    socket: Arc<UtpSocket>,
    congestion_controller: CongestionController,
    connection_id_recv: u16,
    connection_id_send: u16,
    remote_addr: SocketAddr,
    /// Sequence number of the next packet we will send
    seq_number: u16,
    /// Sequence number of the next packet we need to ACK
    ack_number: u16,
    /// Buffer for received but unACKed data
    inbound_data: HashMap<u16, Bytes>,
    /// Buffer for received and ACKed data
    received_data: BytesMut,
    /// Buffer for unsent data
    outbound_data: VecDeque<Bytes>,
    /// Buffer for unprocessed ACKs
    // TODO: Packets should be processed by congestion controller before ending up here
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
            congestion_controller: CongestionController::new(),
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

    fn poll_read_packet(&mut self, cx: &mut Context<'_>) -> Poll<Option<io::Result<Packet>>> {
        let maybe_packet = ready!(self
            .socket
            .packets(self.connection_id_recv(), self.remote_addr())
            .try_poll_next_unpin(cx));

        if let Some(Ok(ref packet)) = maybe_packet {
            self.congestion_controller.update_state(packet);
        }

        Poll::Ready(maybe_packet)
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
        // TODO: Fill out the rest of the packet fields
        #[rustfmt::skip]
        let packet = Packet::new(packet_type, 1, self.connection_id_send(), current_micros(),
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
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // 1. Read as many packets as possible
        while let Poll::Ready(option) = self.poll_read_packet(cx) {
            if let Some(result) = option {
                let packet = result?;
                // TODO: This updates existing inbound data with this seq_number. Is this ok?
                // TODO: What if we're inserting data from a resent packet we just ACKed a little
                // while ago?
                self.inbound_data.insert(packet.seq_number, packet.data);
            } else {
                // Packet stream has terminated, so indicate EOF
                // Set a flag on self so we always return this from now on?
                return Poll::Ready(Ok(()));
            }
        }

        // 2. Assemble as much data as possible, in order
        let mut expected_ack_num = self.ack_number.wrapping_add(1);
        while let Some(data) = self.inbound_data.remove(&expected_ack_num) {
            // TODO: Decrease local receive window
            self.received_data.extend_from_slice(&data);
            // Set ack_number to sequence number of last received packet
            self.ack_number = expected_ack_num;
            expected_ack_num = expected_ack_num.wrapping_add(1);
        }

        // 3. Try sending an ACK for the last packet we got
        ready!(self.poll_send_ack(cx))?;
        self.ack_number = self.ack_number.wrapping_add(1);

        // 4. ACK sent, write data to buffer if we have any
        if self.received_data.is_empty() {
            // TODO: Schedule wakeup? Go back to beginning?
            Poll::Pending
        } else {
            let chunk = if self.received_data.len() > buf.remaining() {
                self.received_data.split_to(buf.remaining())
            } else {
                self.received_data.split()
            };

            debug_assert!(chunk.len() <= buf.remaining());
            buf.put_slice(&chunk);

            // TODO: Increase local receive window

            Poll::Ready(Ok(()))
        }
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
