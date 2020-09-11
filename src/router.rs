use std::{collections::HashMap, net::SocketAddr, sync::RwLock};

use flume::Sender;

use crate::{
    error::*,
    packet::{Packet, PacketType},
};

pub struct Router {
    connection_states: RwLock<HashMap<u16, Sender<(Packet, SocketAddr)>>>,
    syn_packet_tx: Option<Sender<(Packet, SocketAddr)>>,
}

impl Router {
    pub fn new(
        connection_states: RwLock<HashMap<u16, Sender<(Packet, SocketAddr)>>>,
        syn_packet_tx: Option<Sender<(Packet, SocketAddr)>>,
    ) -> Self {
        Self {
            connection_states,
            syn_packet_tx,
        }
    }

    pub fn register_channel(&self, state: Sender<(Packet, SocketAddr)>) -> Result<u16> {
        let mut states = self.connection_states.write().unwrap();
        let mut connection_id = 0;
        while states.contains_key(&connection_id) {
            connection_id = connection_id
                .checked_add(1)
                .ok_or_else(|| Error::TooManyConnections)?;
        }
        debug_assert!(states.insert(connection_id, state).is_none());
        Ok(connection_id)
    }

    pub fn set_channel(&self, id: u16, state: Sender<(Packet, SocketAddr)>) -> bool {
        let mut states = self.connection_states.write().unwrap();
        if states.contains_key(&id) {
            false
        } else {
            debug_assert!(states.insert(id, state).is_none());
            true
        }
    }

    pub fn route(&self, packet: Packet, addr: SocketAddr) {
        match self
            .connection_states
            .read()
            .unwrap()
            .get(&packet.connection_id)
        {
            Some(sender) => {
                match sender.try_send((packet, addr)) {
                    Ok(()) => {}
                    Err(_) => {} // TODO: The receiver is full/gone
                }
            }
            None => {
                if let PacketType::Syn = packet.packet_type {
                    if let Some(tx) = &self.syn_packet_tx {
                        match tx.try_send((packet, addr)) {
                            Ok(()) => {}
                            Err(_) => {} // TODO: The receiving end must be full/closed
                        }
                    }
                } else {
                    // TODO: Not a SYN, and we have no state. Send a reset packet?
                }
            }
        }
    }
}
