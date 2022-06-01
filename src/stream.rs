use std::{
    collections::BTreeMap,
    fmt, io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use futures_util::{ready, stream::TryStreamExt};
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

// Sketch of functionality:
//
// We can worry about selective ACK later, for now just ACK incoming packets and process ACKs for
// sent packets one by one.
//
// For writing, we will be sending data packets and receiving ACKs.
//
// For poll_flush, use the following algorithm:
//   1. Return Poll::Ready if self.outbound_data is empty
//   2. Check inbound ACK packet buffer to see if there's one w/ ack_number = self.seq_number
//     2a. If so, remove the first chunk from self.outbound_data and increment self.seq_number
//     2b. Remove more chunks if there are more ACK packets (or any in the selective ACK extension)
//   3. Poll the socket for a new packet and perform the following:
//     3a. If pending, poll_send the first chunk in self.outbound_data to the remote address
//     3b. If we got a state packet, add it to the incoming ACK buffer
//     3c. If we got a data packet, we must be in the middle of receiving some data as well. Add it
//         to self.inbound_data so poll_read can use it
//   4. Go to step 1
//
// TODO: poll_shutdown
//
// TODO: Need to figure out a plan to deal with lost packets
#[allow(dead_code)]
pub struct UtpStream {
    socket: Arc<UtpSocket>,
    connection_id_recv: u16,
    connection_id_send: u16,
    remote_addr: SocketAddr,
    // TODO: Track connection state (connected, shutdown, etc.)
    seq_number: u16,
    ack_number: u16,
    // TODO: Might be able to just use HashMap
    inbound_data: BTreeMap<u16, Bytes>,
    received_data: BytesMut,
    outbound_data: Vec<Bytes>,
    // TODO: Might be able to just use HashMap
    inbound_acks: BTreeMap<u16, Packet>,
}

impl UtpStream {
    pub(crate) fn new(
        socket: Arc<UtpSocket>,
        connection_id_recv: u16,
        connection_id_send: u16,
        remote_addr: SocketAddr,
        seq_number: u16,
        ack_number: u16,
        inbound_data: BTreeMap<u16, Bytes>,
        received_data: BytesMut,
        outbound_data: Vec<Bytes>,
        inbound_acks: BTreeMap<u16, Packet>,
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
    //     2b. (selective ACK) Add data from any other packets in the buffer with consecutive
    //         seq_numbers?
    //   3. Poll the socket for a new packet and perform the following:
    //     3a. If pending, poll_send an ACK for the last received data packet
    //     3b. If we got a data packet, add to the data packet buffer
    //     3c. If we got a state packet, we must have sent some data earlier. Add it to
    //         self.inbound_acks so poll_flush can use it
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
                    ready!(self.socket.poll_send_to(cx, todo!(), self.remote_addr()))?;
                }
                Poll::Ready(Some(result)) => {
                    let packet = result?;
                    match packet.packet_type {
                        PacketType::Data => {
                            // Save the new packet

                            // TODO: What if we already have this packet?
                            self.inbound_data.insert(packet.seq_number, packet.data);
                        }
                        PacketType::State => {
                            // Must be in the middle of flushing, add this to inbound_acks so
                            // poll_flush can use it.

                            // TODO: What if we already have this packet?
                            self.inbound_acks.insert(packet.seq_number, packet);
                        }
                        _ => {
                            // TODO: Respond to other packet types
                            todo!()
                        }
                    }
                }
                Poll::Ready(None) => {
                    // The stream has terminated, so indicate EOF
                    return Poll::Ready(Ok(()));
                }
            }

            // 4. Go back to step 1
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

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        todo!()
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // TODO: We could set a shutdown flag on UtpStream and return Ok(0) for any future calls to
        //       poll_write, effectively preventing any more packets from being sent
        todo!()
    }
}
