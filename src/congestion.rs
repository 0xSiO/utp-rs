use crate::{packet::Packet, time::current_micros};

/// This is the initial receive buffer capacity used by libtorrent
pub const LOCAL_RECEIVE_WINDOW_SIZE: u32 = 1024 * 1024;

#[allow(dead_code)]
pub struct CongestionController {
    /// Number of bytes currently in-flight (sent but not yet ACKed)
    current_window: u32,
    /// Maximum number of bytes the local socket can currently receive
    local_recieve_window: u32,
    /// Maximum number of bytes the remote socket can currently receive
    remote_receive_window: u32,
    /// Local lowest-ever seen timestamp difference in microseconds
    local_base_delay: u32,
    /// Remote lowest-ever seen timestamp difference in microseconds
    remote_base_delay: u32,
    /// Differences between received packet timestamp delay and the local base delay
    local_delay_samples: Vec<u32>,
    /// Differences between sent packet timestamp delay and the remote base delay
    remote_delay_samples: Vec<u32>,
}

impl CongestionController {
    pub fn new() -> Self {
        Self {
            current_window: 0,
            local_recieve_window: LOCAL_RECEIVE_WINDOW_SIZE,
            remote_receive_window: 0,
            local_base_delay: u32::MAX,
            remote_base_delay: u32::MAX,
            local_delay_samples: vec![],
            remote_delay_samples: vec![],
        }
    }

    pub fn update_state(&mut self, received_packet: &Packet) {
        // TODO: Double check that underflow isn't going to do anything weird
        let local_delay = current_micros().wrapping_sub(received_packet.timestamp_micros);
        let remote_delay = received_packet.timestamp_delta_micros;

        // Update base delays if these are the lowest seen
        self.local_base_delay = self.local_base_delay.min(local_delay);
        self.remote_base_delay = self.remote_base_delay.min(remote_delay);

        // Update delay sample histories
        self.local_delay_samples.push(local_delay);
        self.remote_delay_samples.push(remote_delay);

        self.remote_receive_window = received_packet.window_size;
    }
}
