use std::{collections::HashMap, convert::TryFrom, net::SocketAddr};

use bytes::{Bytes, BytesMut};
use crossbeam_queue::SegQueue;
use log::{debug, trace};
use tokio::{
    net::{lookup_host, ToSocketAddrs, UdpSocket},
    sync::{Mutex, RwLock},
};

use crate::{error::*, packet::Packet};

// Ethernet MTU minus IP/UDP header sizes. TODO: Use path MTU discovery
const MAX_DATAGRAM_SIZE: usize = 1472;

#[derive(Debug)]
pub struct UtpSocket {
    socket: Mutex<UdpSocket>,
    connection_states: RwLock<HashMap<u16, SegQueue<(Packet, SocketAddr)>>>,
    syn_packets: SegQueue<(Packet, SocketAddr)>,
    local_addr: SocketAddr,
    // Maximum number of bytes the socket may have in-flight at any given time
    // max_window: u32,
    // Number of bytes currently in-flight
    // local_window: u32,
    // Upper limit of the number of in-flight bytes, as given by remote peer
    // remote_window: u32,
}

impl UtpSocket {
    pub fn new(
        socket: Mutex<UdpSocket>,
        connection_states: RwLock<HashMap<u16, SegQueue<(Packet, SocketAddr)>>>,
        syn_packets: SegQueue<(Packet, SocketAddr)>,
        local_addr: SocketAddr,
    ) -> Self {
        Self {
            socket,
            connection_states,
            syn_packets,
            local_addr,
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn bind(local_addr: impl ToSocketAddrs) -> Result<Self> {
        let udp_socket = UdpSocket::bind(local_addr).await?;
        let local_addr = udp_socket.local_addr()?;
        trace!("binding to {}", local_addr);
        Ok(Self::new(
            Mutex::new(udp_socket),
            Default::default(),
            Default::default(),
            local_addr,
        ))
    }

    pub async fn get_packet(&self, connection_id: u16, remote_addr: SocketAddr) -> Result<Packet> {
        loop {
            if let Some(queue) = self.connection_states.read().await.get(&connection_id) {
                while let Ok((packet, addr)) = queue.pop() {
                    if addr == remote_addr {
                        return Ok(packet);
                    }
                    // Drop packet from unknown source
                }
            }

            let (packet, addr) = self.recv_from().await?;
            if packet.connection_id == connection_id && addr == remote_addr {
                return Ok(packet);
            } else {
                self.connection_states
                    .write()
                    .await
                    .entry(packet.connection_id)
                    .or_default()
                    .push((packet, addr));
            }
        }
    }

    pub async fn send_to(&self, packet: Packet, remote_addr: impl ToSocketAddrs) -> Result<usize> {
        let remote_addr = lookup_host(remote_addr)
            .await?
            .next()
            .ok_or_else(|| Error::MissingAddress)?;
        debug!(
            "{} -> {} {:?}",
            self.local_addr, remote_addr, packet.packet_type
        );
        Ok(self
            .socket
            .lock()
            .await
            .send_to(&Bytes::from(packet), remote_addr)
            .await?)
    }

    pub async fn recv_from(&self) -> Result<(Packet, SocketAddr)> {
        let mut buf = BytesMut::with_capacity(MAX_DATAGRAM_SIZE);
        buf.resize(MAX_DATAGRAM_SIZE, 0);
        let (bytes_read, remote_addr) = self.socket.lock().await.recv_from(&mut buf).await?;
        buf.truncate(bytes_read);
        let packet = Packet::try_from(buf.freeze())?;
        debug!(
            "{} <- {} {:?} ({} bytes)",
            self.local_addr, remote_addr, packet.packet_type, bytes_read
        );
        Ok((packet, remote_addr))
    }
}
