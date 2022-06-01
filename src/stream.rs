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
    error::*,
    packet::{Packet, PacketType},
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
        // TODO: Figure out timestamp, timestamp_delta, and receive window size
        let packet = Packet::new(
            PacketType::State,
            1,
            self.connection_id_send(),
            0,
            0,
            0,
            self.seq_number,
            self.ack_number,
            vec![],
            Bytes::new(),
        );

        // TODO: Add sent bytes to current window size
        let _ = ready!(self.socket.poll_send_to(cx, packet, self.remote_addr()))?;
        Poll::Ready(Ok(()))
    }

    fn poll_send_data(&self, cx: &mut Context<'_>, data: Bytes) -> Poll<io::Result<()>> {
        // TODO: Figure out timestamp, timestamp_delta, and receive window size
        let packet = Packet::new(
            PacketType::Data,
            1,
            self.connection_id_send(),
            0,
            0,
            0,
            self.seq_number,
            self.ack_number,
            vec![],
            data,
        );

        // TODO: Add sent bytes to current window size
        let _ = ready!(self.socket.poll_send_to(cx, packet, self.remote_addr()))?;
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

impl AsyncRead for UtpStream {
    // For reading, we will be receiving data packets and sending ACKs.
    // We use the following algorithm:
    //   1. Return Poll::Ready with data from self.received_data if there's any data in the buffer
    //   2. Check inbound packet buffer to see if there's one w/ seq_number = self.ack_number + 1
    //     2a. If so, update self.ack_number and add the data to the self.received_data buffer
    //   3. Poll the socket for a new packet and perform the following:
    //     3a. If pending, poll_send an ACK for the last received data packet
    //     3b. If we got a packet, handle it or save it to the correct buffer depending on its type
    //   4. Go to step 1
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            // 1. If we have data sitting in our buffer, write to buf and return Poll::Ready
            if !self.received_data.is_empty() {
                let chunk = if self.received_data.len() > buf.remaining() {
                    self.received_data.split_to(buf.remaining())
                } else {
                    self.received_data.split()
                };

                debug_assert!(chunk.len() <= buf.remaining());
                buf.put_slice(&chunk);
                return Poll::Ready(Ok(()));
            }

            // 2. We don't have any data right now, so check the inbound data packet buffer
            let expected_seq_num = self.ack_number.wrapping_add(1);
            debug!(
                "Reader expecting seq_number {} (self.seq_number = {}, self.ack_number = {})",
                expected_seq_num, self.seq_number, self.ack_number
            );
            if let Some(chunk) = self.inbound_data.remove(&expected_seq_num) {
                // Nice, grab the data and go back to step 1
                self.ack_number = expected_seq_num;
                self.received_data.extend_from_slice(&chunk);
                continue;
            }

            // 3. We have no inbound or received data, so poll for a packet
            let maybe_next_packet = self
                .socket
                .packets(self.connection_id_recv(), self.remote_addr())
                .try_poll_next_unpin(cx);
            match maybe_next_packet {
                Poll::Pending => {
                    // No packets available yet. Try ACKing the last packet we expected to receive
                    // TODO: Remove this once I'm done debugging :)
                    std::thread::sleep_ms(500);
                    ready!(self.poll_send_ack(cx))?;
                }
                Poll::Ready(Some(result)) => self.handle_packet(result?),
                Poll::Ready(None) => {
                    // The stream has terminated, so indicate EOF
                    return Poll::Ready(Ok(()));
                }
            }

            // 4. Go back to step 1
        }
    }
}

// TODO: Do some more research on how packets should actually be sent/received :)
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

    // For writing, we will be sending data packets and receiving ACKs.
    // We use the following algorithm:
    //   1. Return Poll::Ready if self.outbound_data is empty
    //   2. Check inbound ACK packet buffer to see if there's one w/ ack_number = self.seq_number
    //     2a. If so, remove the first chunk from self.outbound_data and increment self.seq_number
    //   3. Poll the socket for a new packet and perform the following:
    //     3a. If pending, poll_send the first chunk in self.outbound_data to the remote address
    //     3b. If we got a packet, handle it or save it to the correct buffer depending on its type
    //   4. Go to step 1
    //
    // TODO: Any extra required logic to deal with duplicate ACKs and lost packets
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            // 1. If we have no more outbound data, we're finished
            if self.outbound_data.is_empty() {
                return Poll::Ready(Ok(()));
            }

            // 2. We still have unACKed packets, so check to see if we've received any ACKs
            let expected_ack_num = self.seq_number;
            debug!(
                "Writer expecting ack_number {} (self.seq_number = {}, self.ack_number = {})",
                expected_ack_num, self.seq_number, self.ack_number
            );
            if let Some(_packet) = self.inbound_acks.remove(&expected_ack_num) {
                // Nice, remove the next chunk from our queue and go back to step 1
                self.seq_number = self.seq_number.wrapping_add(1);
                self.outbound_data.pop_front();
                continue;
            }

            // 3. No ACKs yet for the next packet, so poll for a new packet
            let maybe_next_packet = self
                .socket
                .packets(self.connection_id_recv(), self.remote_addr())
                .try_poll_next_unpin(cx);
            match maybe_next_packet {
                Poll::Pending => {
                    // No packets available yet. Try sending the next packet in the outbound queue
                    // Ok to unwrap, at this point outbound_data should not be empty
                    let data = self.outbound_data.front().unwrap().clone();
                    // TODO: Remove this once I'm done debugging :)
                    std::thread::sleep_ms(500);
                    ready!(self.poll_send_data(cx, data))?;
                }
                Poll::Ready(Some(result)) => self.handle_packet(result?),
                Poll::Ready(None) => {
                    // TODO: Stream has terminated, return an error?
                    todo!()
                }
            }

            // 4. Go back to step 1
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_flush(cx))?;
        // TODO: We could set a shutdown flag on UtpStream and return Ok(0) for any future calls to
        //       poll_write, effectively preventing any more packets from being sent
        todo!()
    }
}
