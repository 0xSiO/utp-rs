use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Weak,
    task::{Context, Poll},
};

use bytes::Bytes;
use dashmap::DashMap;
use log::{debug, error, trace, warn};
use tokio::{io::ReadBuf, net::UdpSocket, sync::mpsc::UnboundedSender};

use crate::packet::{Packet, PacketType};

pub(crate) struct PacketReceiver {
    pub(crate) socket: Weak<UdpSocket>,
    pub(crate) local_addr: SocketAddr,
    pub(crate) syn_tx: UnboundedSender<(Packet, SocketAddr)>,
    pub(crate) routing_table: Weak<DashMap<(u16, SocketAddr), UnboundedSender<Packet>>>,
}

impl Future for PacketReceiver {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Try to access the socket
        let socket = match self.socket.upgrade() {
            Some(socket) => {
                trace!(
                    "({} recv task) socket exists, about to read packets",
                    self.local_addr
                );
                socket
            }
            // Socket was dropped, so we can shut down.
            None => {
                trace!(
                    "({} recv task) socket dropped, shutting down task",
                    self.local_addr
                );
                return Poll::Ready(());
            }
        };

        // Read and process as many packets as possible
        loop {
            match socket.poll_recv_ready(cx) {
                Poll::Ready(Ok(())) => {
                    let mut buf = [0; super::MAX_DATAGRAM_SIZE];
                    let mut buf = ReadBuf::new(&mut buf);
                    match socket.poll_recv_from(cx, &mut buf) {
                        Poll::Ready(Ok(remote_addr)) => {
                            let packet =
                                match Packet::try_from(Bytes::copy_from_slice(buf.filled())) {
                                    Ok(packet) => packet,
                                    Err(err) => {
                                        warn!(
                                            "({} recv task) got invalid packet ({}), ignoring",
                                            self.local_addr, err
                                        );
                                        continue;
                                    }
                                };

                            // Route the packet
                            if let PacketType::Syn = packet.packet_type {
                                match self.syn_tx.send((packet, remote_addr)) {
                                    Ok(()) => trace!(
                                        "({} recv task) saved SYN packet from {}",
                                        self.local_addr,
                                        remote_addr
                                    ),
                                    // Syn packet receiver (and thus the socket) was dropped, so we can shut down.
                                    Err(_) => {
                                        trace!(
                                            "({} recv task) socket dropped, shutting down task",
                                            self.local_addr
                                        );
                                        return Poll::Ready(());
                                    }
                                }
                            } else {
                                let routing_table = match self.routing_table.upgrade() {
                                    Some(routing_table) => routing_table,
                                    // Routing table (and thus the socket) was dropped, so we can shut down.
                                    None => {
                                        trace!(
                                            "({} recv task) socket dropped, shutting down task",
                                            self.local_addr
                                        );
                                        return Poll::Ready(());
                                    }
                                };

                                let mut found_conn = false;
                                routing_table.remove_if(
                                    &(packet.connection_id, remote_addr),
                                    |_, sender| {
                                        found_conn = true;
                                        match sender.send(packet.clone()) {
                                            Ok(()) => {
                                                debug!(
                                                    "Conn #{}: {} <- {} {:?} ({} bytes)",
                                                    packet.connection_id,
                                                    self.local_addr,
                                                    remote_addr,
                                                    packet.packet_type,
                                                    buf.filled().len()
                                                );
                                                false
                                            }
                                            Err(_) => {
                                                trace!(
                                                    "({} recv task) dropping stale connection #{} to {}",
                                                    self.local_addr,
                                                    packet.connection_id,
                                                    remote_addr
                                                );
                                                true
                                            }
                                        }
                                    },
                                );

                                if !found_conn {
                                    trace!(
                                        "({} recv task) no connection found for {:?} packet from {}", 
                                        self.local_addr,
                                        packet.packet_type,
                                        remote_addr
                                    );
                                }
                            }
                        }
                        Poll::Ready(Err(err)) => {
                            warn!(
                                "({} recv task) error reading from socket ({}), retrying",
                                self.local_addr, err
                            );
                            continue;
                        }
                        Poll::Pending => {
                            trace!(
                                "({} recv task) socket isn't ready, going to sleep",
                                self.local_addr
                            );
                            return Poll::Pending;
                        }
                    }
                }
                Poll::Ready(Err(err)) => {
                    error!(
                        "({} recv task) poll_recv_ready returned error ({}), shutting down",
                        self.local_addr, err
                    );
                    // The IO driver might have terminated, in which case we can't do much else
                    return Poll::Ready(());
                }
                Poll::Pending => {
                    trace!(
                        "({} recv task) socket isn't ready, going to sleep",
                        self.local_addr
                    );
                    return Poll::Pending;
                }
            }
        }
    }
}
