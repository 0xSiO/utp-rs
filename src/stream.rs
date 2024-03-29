use std::{
    collections::{HashMap, VecDeque},
    fmt, io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use futures_util::ready;
use log::debug;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{lookup_host, ToSocketAddrs},
    sync::mpsc::UnboundedReceiver,
};

use crate::{
    congestion::CongestionController,
    error::ConnectionError,
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
    inbound_packets: UnboundedReceiver<Packet>,
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
    is_shutdown: bool,
}

impl UtpStream {
    pub(crate) fn new(
        socket: Arc<UtpSocket>,
        connection_id_recv: u16,
        connection_id_send: u16,
        remote_addr: SocketAddr,
        inbound_packets: UnboundedReceiver<Packet>,
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
            inbound_packets,
            seq_number,
            ack_number,
            inbound_data,
            received_data,
            unacked_data,
            is_shutdown: false,
        }
    }

    /// Create a new [`UtpStream`] using the given socket, and attempt to connect the [`UtpStream`]
    /// to the provided address.
    pub async fn connect(
        socket: Arc<UtpSocket>,
        remote_addr: impl ToSocketAddrs,
    ) -> io::Result<Self> {
        let remote_addr = lookup_host(remote_addr)
            .await?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, ConnectionError::NoAddress))?;

        let (connection_id_recv, mut receiver) = socket.register_connection(remote_addr);
        let connection_id_send = connection_id_recv.wrapping_add(1);
        let seq_number = 1;
        // Just set ack_number to 0, this shouldn't be read by the remote socket anyway
        let ack_number = 0;
        // TODO: Set other packet fields
        #[rustfmt::skip]
        let syn = Packet::new(PacketType::Syn, 1, connection_id_recv, 0, 0, 0, seq_number,
                              ack_number, vec![], Bytes::new());
        let seq_number = seq_number.wrapping_add(1);
        socket.send_to(syn, remote_addr)?;

        // state: SYN sent

        // TODO: Handle this more gracefully
        let response_packet = receiver.recv().await.unwrap();

        match response_packet.packet_type {
            PacketType::State => {
                // state: connected
                Ok(Self::new(
                    socket,
                    connection_id_recv,
                    connection_id_send,
                    remote_addr,
                    receiver,
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

    pub(crate) fn inbound_packets(&mut self) -> &mut UnboundedReceiver<Packet> {
        &mut self.inbound_packets
    }

    /// Attempt to read a packet from the inbound packet channel. If this returns
    /// Poll::Ready(None), the stream has been shut down.
    fn poll_read_packet(&mut self, cx: &mut Context<'_>) -> Poll<Option<Packet>> {
        loop {
            let maybe_packet = ready!(self.inbound_packets.poll_recv(cx));

            match maybe_packet {
                Some(ref packet) => {
                    // We want to create this timestamp as close to receipt time as possible
                    let received_at = current_micros();

                    // Just ignore the packet if it's suspicious
                    // TODO: Maybe close the connection after a certain number of suspicious packets
                    if self.is_suspicious(packet) {
                        continue;
                    }

                    self.congestion_controller.update_state(received_at, packet);
                }
                None => self.is_shutdown = true,
            }

            return Poll::Ready(maybe_packet);
        }
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
                // TODO: Should we drop unACKed packets that we've already received?
                // libutp just discards duplicates
                self.inbound_data.insert(packet.seq_number, packet.data);
            }
            PacketType::State => {
                // Remove any data that has now been ACKed
                while let Some((oldest_seq_num, data)) = self.unacked_data.pop_front() {
                    // TODO: Be more precise about this comparison (account for overflow?)
                    if oldest_seq_num <= packet.ack_number {
                        drop((oldest_seq_num, data));
                    } else {
                        // Oops, put it back - we haven't gotten an ACK for this one yet
                        self.unacked_data.push_front((oldest_seq_num, data));
                        break;
                    }
                }
            }
            // TODO: Respond to other packet types
            _ => todo!(),
        }
    }

    fn send_packet(
        &self,
        packet_type: PacketType,
        seq_number: u16,
        ack_number: u16,
        extensions: Vec<Extension>,
        data: Bytes,
    ) -> io::Result<()> {
        // TODO: Fill out the rest of the packet fields
        #[rustfmt::skip]
        let packet = Packet::new(packet_type, 1, self.connection_id_send(), current_micros(),
                                 0, 0, seq_number, ack_number, extensions, data);
        // TODO: Error handling?
        self.socket.send_to(packet, self.remote_addr())
    }

    fn send_ack(&self) -> io::Result<()> {
        self.send_packet(
            PacketType::State,
            self.seq_number,
            self.ack_number,
            vec![],
            Bytes::new(),
        )
    }

    fn send_data(&mut self, data: Bytes) -> io::Result<()> {
        self.send_packet(
            PacketType::Data,
            self.seq_number,
            self.ack_number,
            vec![],
            data.clone(),
        )?;

        self.unacked_data.push_back((self.seq_number, data));
        self.seq_number = self.seq_number.wrapping_add(1);
        // TODO: Update current window here?
        Ok(())
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
                Some(packet) => self.handle_packet(packet),
                // Packet stream has terminated, so indicate EOF
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
        self.send_ack()?;

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
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.is_shutdown {
            return Poll::Ready(Ok(0));
        }

        let mut bytes_written = 0;
        for chunk in buf.chunks(MAX_DATA_SEGMENT_SIZE) {
            // TODO: Don't copy each chunk, use Bytes::split_to to split up the data
            match self.send_data(Bytes::copy_from_slice(chunk)) {
                Ok(_) => bytes_written += chunk.len(),
                Err(err) => return Poll::Ready(Err(err)),
            }
        }

        Poll::Ready(Ok(bytes_written))
    }

    // TODO: Any extra required logic to deal with duplicate ACKs and lost packets
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // 1. Process as many packets as possible
        while let Poll::Ready(option) = self.poll_read_packet(cx) {
            match option {
                Some(packet) => self.handle_packet(packet),
                // Packet stream has terminated
                None => break,
            }
        }

        // 2. Return if there's no more unACKed data
        if self.unacked_data.is_empty() {
            Poll::Ready(Ok(()))
        } else if self.is_shutdown {
            // TODO: Return an error since we have unACKed data
            todo!()
        } else {
            Poll::Pending
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.is_shutdown = true;
        ready!(self.poll_flush(cx))?;
        // TODO: Close self.inbound_packets. May need a poll_flush_priv method so self is not
        //       consumed
        // self.inbound_packets.close();
        todo!()
    }
}
