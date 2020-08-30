use std::net::SocketAddr;

use dashmap::{DashMap, ElementGuard};
use tokio::sync::mpsc::UnboundedSender;

use crate::packet::{Packet, PacketType};

// TODO: Do we need this struct?
pub struct ConnectionState {
    packet_tx: UnboundedSender<(Packet, SocketAddr)>,
}

impl ConnectionState {
    pub fn new(packet_tx: UnboundedSender<(Packet, SocketAddr)>) -> Self {
        Self { packet_tx }
    }
}

pub struct ConnectionManager {
    connection_states: DashMap<u16, ConnectionState>,
    syn_packet_tx: UnboundedSender<(Packet, SocketAddr)>,
}

impl ConnectionManager {
    pub fn new(
        connection_states: DashMap<u16, ConnectionState>,
        syn_packet_tx: UnboundedSender<(Packet, SocketAddr)>,
    ) -> Self {
        Self {
            connection_states,
            syn_packet_tx,
        }
    }

    pub fn get_state(&self, id: u16) -> Option<ElementGuard<u16, ConnectionState>> {
        self.connection_states.get(&id)
    }

    pub fn set_state(&self, id: u16, state: ConnectionState) -> bool {
        if self.connection_states.contains_key(&id) {
            false
        } else {
            // TODO: This could cause a bug where state is mysteriously overridden
            //       If this is a problem, consider using Arc<Mutex<HashMap>>
            let success = self.connection_states.insert(id, state);
            // Make absolutely sure the state didn't already exist
            debug_assert!(success);
            true
        }
    }

    pub fn route(&self, packet: Packet, addr: SocketAddr) {
        match self.connection_states.get(&packet.connection_id) {
            Some(state) => {
                match state.value().packet_tx.send((packet, addr)) {
                    Ok(()) => {}
                    Err(_) => {} // TODO: The receiver is gone, so this state is stale?
                }
            }
            None => {
                if let PacketType::Syn = packet.packet_type {
                    match self.syn_packet_tx.send((packet, addr)) {
                        Ok(()) => {}
                        Err(_) => {} // TODO: The receiving end must be closed. Log this?
                    }
                } else {
                    // TODO: Not a SYN, and we have no state. Send a reset packet?
                }
            }
        }
        todo!()
    }
}
