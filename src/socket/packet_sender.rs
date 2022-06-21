use std::{
    collections::VecDeque,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Weak,
    task::{Context, Poll},
};

use bytes::Bytes;
use log::{debug, trace, warn};
use tokio::{net::UdpSocket, sync::mpsc::UnboundedReceiver};

use crate::packet::Packet;

pub(crate) struct PacketSender {
    pub(crate) socket: Weak<UdpSocket>,
    pub(crate) local_addr: SocketAddr,
    pub(crate) outgoing_rx: UnboundedReceiver<(Packet, SocketAddr)>,
    pub(crate) outgoing_buffer: VecDeque<(Packet, SocketAddr)>,
}

impl Future for PacketSender {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // First, fetch as many packets as possible from the outgoing packet channel
        while let Poll::Ready(option) = self.outgoing_rx.poll_recv(cx) {
            match option {
                Some(item) => self.outgoing_buffer.push_back(item),
                // If the send half of the outgoing packet channel was dropped, that means
                // the socket was dropped. If the socket was dropped, we can shut down.
                None => {
                    trace!(
                        "({} send task) socket dropped, shutting down task",
                        self.local_addr
                    );
                    return Poll::Ready(());
                }
            }
        }

        trace!(
            "({} send task) {} packets in outgoing buffer",
            self.local_addr,
            self.outgoing_buffer.len(),
        );

        // At this point we're scheduled for wakeup once another message shows up in outgoing_rx OR
        // when the channel is closed.

        // Try to access the socket
        let socket = match self.socket.upgrade() {
            Some(socket) => {
                trace!(
                    "({} send task) socket exists, about to send packets",
                    self.local_addr
                );
                socket
            }
            // Socket was dropped, so we can shut down.
            None => {
                trace!(
                    "({} send task) socket dropped, shutting down task",
                    self.local_addr
                );
                return Poll::Ready(());
            }
        };

        // Socket is still alive, so send out as many packets as we can
        while let Some((packet, remote_addr)) = self.outgoing_buffer.pop_front() {
            match socket.poll_send_to(cx, &Bytes::from(packet.clone()), remote_addr) {
                Poll::Ready(Ok(bytes_written)) => {
                    debug!(
                        "Conn #{}: {} -> {} {:?} ({} bytes)",
                        packet.connection_id,
                        self.local_addr,
                        remote_addr,
                        packet.packet_type,
                        bytes_written
                    );
                }
                Poll::Ready(Err(err)) => {
                    // On failure, put the packet back and try again
                    warn!(
                        "Conn #{}: {} -> {} {:?} FAILED ({})",
                        packet.connection_id, self.local_addr, remote_addr, packet.packet_type, err
                    );
                    self.outgoing_buffer.push_front((packet, remote_addr));
                }
                Poll::Pending => {
                    trace!(
                        "({} send task) socket isn't ready, going to sleep",
                        self.local_addr
                    );

                    // Put the packet back and wait until the socket is ready to write.
                    // We could also be woken up if a packet arrives in outgoing_rx.
                    self.outgoing_buffer.push_front((packet, remote_addr));
                    return Poll::Pending;
                }
            }
        }

        trace!(
            "({} send task) all packets sent, going to sleep",
            self.local_addr
        );

        // No more packets to send, so wait for more to be sent through the channel. We're already
        // scheduled for wakeup from calling poll_recv on outgoing_rx earlier.
        Poll::Pending
    }
}
