use std::net::SocketAddr;

use flurry::HashMap;
use tokio::sync::mpsc::UnboundedSender;

use crate::packet::{Packet, PacketType};

pub struct Router {
    connection_states: HashMap<u16, UnboundedSender<(Packet, SocketAddr)>>,
    syn_packet_tx: UnboundedSender<(Packet, SocketAddr)>,
}

impl Router {
    pub fn new(
        connection_states: HashMap<u16, UnboundedSender<(Packet, SocketAddr)>>,
        syn_packet_tx: UnboundedSender<(Packet, SocketAddr)>,
    ) -> Self {
        Self {
            connection_states,
            syn_packet_tx,
        }
    }

    pub fn has_channel(&self, id: u16) -> bool {
        self.connection_states.pin().contains_key(&id)
    }

    pub fn set_channel(&self, id: u16, state: UnboundedSender<(Packet, SocketAddr)>) -> bool {
        self.connection_states.pin().try_insert(id, state).is_ok()
    }

    pub fn route(&self, packet: Packet, addr: SocketAddr) {
        match self.connection_states.pin().get(&packet.connection_id) {
            Some(sender) => {
                match sender.send((packet, addr)) {
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
