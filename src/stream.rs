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
    /// Buffer for received but unACKed data. Entries are (seq_number, data)
    inbound_data: HashMap<u16, Bytes>,
    /// Buffer for received and ACKed data
    received_data: BytesMut,
    /// Queue for in-flight data (sent but unACKed). Elements are (seq_number, data)
    unacked_data: VecDeque<(u16, Bytes)>,
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
        unacked_data: VecDeque<(u16, Bytes)>,
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
            unacked_data,
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
            // We want to create this timestamp as close to receipt time as possible
            let received_at = current_micros();
            if !self.is_suspicious(packet) {
                self.congestion_controller.update_state(received_at, packet);
            }
        }

        Poll::Ready(maybe_packet)
    }

    fn is_suspicious(&self, packet: &Packet) -> bool {
        match packet.packet_type {
            // For data packets, we will consider sequence numbers outside a certain distance
            // from our current ack_number to be suspicious.
            //
            //  0             A = self.ack_number      65535
            //  |---------------><----A----><------------|
            //         sus           ok          sus
            PacketType::Data => {
                // TODO: We'll use 128 for now as an arbitrary estimate, but perhaps this should
                // depend somewhat on the local receive window
                let acceptable_distance = 128;
                let left_distance = self.ack_number.wrapping_sub(packet.seq_number);
                let right_distance = packet.seq_number.wrapping_sub(self.ack_number);
                if left_distance > acceptable_distance && right_distance > acceptable_distance {
                    return true; // kinda sus
                }
                // TODO: Other heuristics?
            }
            // For ACKs, we use a similar heuristic to the one for data packets. If the packet
            // ack_number is greater than or equal to our current seq_number, or is further back
            // than the current send window, we'll consider it suspicious.
            //
            //  0             S = self.seq_number      65535
            //  |--------------><---->S<-----------------|
            //         sus        ok          sus
            PacketType::State => {
                // We'll give some leeway of a few packets too, just in case.
                // TODO: Is 3 a good estimate for now?
                let acceptable_distance = self.unacked_data.len() + 3;
                let left_distance = self.seq_number.wrapping_sub(packet.ack_number) as usize;
                if left_distance == 0 || left_distance > acceptable_distance {
                    return true; // kinda sus
                }
                // TODO: Other heuristics?
            }
            // TODO: Other packet types
            _ => todo!(),
        }

        false
    }

    fn handle_packet(&mut self, packet: Packet) {
        debug!("Handling inbound packet: {:?}", packet);
        match packet.packet_type {
            PacketType::Data => {
                // TODO: What if it's a resent packet we already ACKed a little while ago?
                // Possible solution: Only save new packets if their seq_number falls within a
                // certain distance from self.ack_number. This distance can be estimated by
                // dividing the current local receive window by the bytes per uTP packet.
                // Then if packet.seq_number - self.ack_number > distance, drop the packet.
                //
                // Receive window for all possible packets: ~1400 bytes * u16::MAX, roughly 91 MB.
                // Keep receive window much smaller than that and the estimated distance for
                // acceptable seq_numbers shouldn't cause any issues.
                //
                // |......................A---------E.........|
                // 0                      ^---------^      u16::MAX
                //                         distance
                // A = self.ack_number
                // E = end of acceptable seq_numbers

                let permitted_distance = 128; // We'll use 128 for now as an arbitrary estimate
                if packet.seq_number.wrapping_sub(self.ack_number) <= permitted_distance {
                    // TODO: Should we drop unACKed packets that we've already received?
                    // libutp just discards duplicates
                    self.inbound_data.insert(packet.seq_number, packet.data);
                }
            }
            PacketType::State => {
                // TODO: Drop packets outside an acceptable distance from the current seq_number

                // Remove any data that has now been ACKed
                while let Some((oldest_seq_num, data)) = self.unacked_data.pop_front() {
                    // TODO: Be more precise about this comparison (account for overflow)
                    if oldest_seq_num <= packet.ack_number {
                        drop((oldest_seq_num, data));
                    } else {
                        // Oops, put it back - we haven't gotten an ACK for this one yet
                        self.unacked_data.push_front((oldest_seq_num, data));
                        break;
                    }
                }
            }
            _ => {
                // TODO: Respond to other packet types
                todo!()
            }
        }
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
        Poll::Ready(Ok(()))
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

    fn poll_send_data(&mut self, cx: &mut Context<'_>, data: Bytes) -> Poll<io::Result<()>> {
        ready!(self.poll_send_packet(
            cx,
            PacketType::Data,
            self.seq_number,
            self.ack_number,
            vec![],
            data.clone(),
        ))?;

        self.unacked_data.push_back((self.seq_number, data));
        self.seq_number = self.seq_number.wrapping_add(1);
        // TODO: Update current window here?
        Poll::Ready(Ok(()))
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
//
// Example conversation between two peers, A and B.
// A -> SYN  (Conn 30873, Wnd  262 KB, Seq 23958, Ack     0,   0 bytes) #93
//   Ext 2: bitfield 0x0000000000000000
// B <- ACK  (Conn 30873, Wnd 1048 KB, Seq 21281, Ack 23958,   0 bytes) #108
// A -> DATA (Conn 30874, Wnd  262 KB, Seq 23959, Ack 21280, 520 bytes) #136
// B <- ACK  (Conn 30873, Wnd 1048 KB, Seq 21281, Ack 23959,   0 bytes) #149
// B <- DATA (Conn 30873, Wnd 1048 KB, Seq 21281, Ack 23959, 185 bytes) #151
// A -> ACK  (Conn 30874, Wnd  262 KB, Seq 23960, Ack 21281,   0 bytes) #165
// A -> DATA (Conn 30874, Wnd  262 KB, Seq 23960, Ack 21281, 124 bytes) #180
// B <- ACK  (Conn 30873, Wnd 1048 KB, Seq 21282, Ack 23960,   0 bytes) #205
//
// Some time later...
// B <- DATA (Conn 30873, Wnd 1048 KB, Seq 21287, Ack 23963, 528 bytes) #3953
// B <- DATA (Conn 30873, Wnd 1048 KB, Seq 21288, Ack 23963, 528 bytes) #3954
// B <- DATA (Conn 30873, Wnd 1048 KB, Seq 21289, Ack 23963, 528 bytes) #3955
// B <- DATA (Conn 30873, Wnd 1048 KB, Seq 21290, Ack 23963, 528 bytes) #3956
// A -> ACK  (Conn 30874, Wnd  262 KB, Seq 23964, Ack 21290,   0 bytes) #4004
// (B sends 10 more DATA packets with 528 bytes each. Last Seq 21300, Ack 23963)
// A -> ACK  (Conn 30874, Wnd  262 KB, Seq 23964, Ack 21295,   0 bytes) #4042
// A -> ACK  (Conn 30874, Wnd  262 KB, Seq 23964, Ack 21300,   0 bytes) #4043
// (B sends 5 more DATA packets just like above, Last Seq 21305, Ack 23963)
// A -> ACK  (Conn 30874, Wnd  262 KB, Seq 23964, Ack 21305,   0 bytes) #4070
// ...

impl AsyncRead for UtpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // 1. Process as many packets as possible
        while let Poll::Ready(option) = self.poll_read_packet(cx) {
            match option {
                Some(Ok(packet)) => self.handle_packet(packet),
                Some(Err(err)) => return Poll::Ready(Err(err)),
                // Packet stream has terminated, so indicate EOF
                // TODO: Set a flag on self so we always return this from now on?
                None => return Poll::Ready(Ok(())),
            }
        }

        // 2. Assemble as much data as possible, in order
        let mut next_seq_num = self.ack_number.wrapping_add(1);
        while let Some(data) = self.inbound_data.remove(&next_seq_num) {
            // TODO: Decrease local receive window?
            self.received_data.extend_from_slice(&data);
            // Set ack_number to sequence number of last received packet
            self.ack_number = next_seq_num;
            next_seq_num = next_seq_num.wrapping_add(1);
        }

        // 3. Try sending an ACK for the last packet we got
        ready!(self.poll_send_ack(cx))?;

        // 4. ACK sent, write data to buffer, if any
        if self.received_data.is_empty() {
            // We don't have any data, which means we must have sent a duplicate ACK.
            // TODO: How many of these duplicates should we tolerate before giving up?
            Poll::Pending
        } else {
            // Write as much data to the buffer as possible
            let chunk = if self.received_data.len() > buf.remaining() {
                self.received_data.split_to(buf.remaining())
            } else {
                self.received_data.split()
            };

            debug_assert!(chunk.len() <= buf.remaining());
            buf.put_slice(&chunk);
            // TODO: Increase local receive window?
            Poll::Ready(Ok(()))
        }
    }
}

impl AsyncWrite for UtpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut bytes_written = 0;

        // TODO: Don't copy each chunk, use Bytes::split_to to split up the data
        for chunk in buf.chunks(MAX_DATA_SEGMENT_SIZE) {
            match self.poll_send_data(cx, Bytes::copy_from_slice(chunk)) {
                Poll::Ready(Ok(_)) => bytes_written += chunk.len(),
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                Poll::Pending => {
                    if bytes_written == 0 {
                        // Don't return Poll::Ready(Ok(0)) yet, we may be writable in the future
                        return Poll::Pending;
                    }
                    break;
                }
            }
        }

        Poll::Ready(Ok(bytes_written))
    }

    // TODO: Any extra required logic to deal with duplicate ACKs and lost packets
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // 1. Process as many packets as possible
        while let Poll::Ready(option) = self.poll_read_packet(cx) {
            match option {
                Some(Ok(packet)) => self.handle_packet(packet),
                Some(Err(err)) => return Poll::Ready(Err(err)),
                // Packet stream has terminated, so indicate EOF
                // TODO: Set a flag on self so we always return this from now on?
                None => return Poll::Ready(Ok(())),
            }
        }

        // 2. Return if there's no more unACKed data
        if self.unacked_data.is_empty() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_flush(cx))?;
        // TODO: We could set a shutdown flag on UtpStream and return Ok(0) for any future calls to
        //       poll_write, effectively preventing any more packets from being sent
        todo!()
    }
}
