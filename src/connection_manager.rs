use std::net::SocketAddr;

use dashmap::{DashMap, ElementGuard};
use tokio::sync::mpsc::UnboundedSender;

use crate::{connection::ConnectionState, Packet};

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

    pub fn route(&self, packet: Packet) {
        todo!()
    }
}
