use crate::packet::Packet;

/// This is the initial receive buffer capacity used by libtorrent
const LOCAL_RECEIVE_WINDOW_SIZE: u32 = 1024 * 1024;
const ETHERNET_MTU: u32 = 1500;

#[allow(dead_code)]
pub struct CongestionController {
    /// Number of bytes currently in-flight (sent but not yet ACKed)
    current_window: u32,
    /// Maximum number of bytes we can currently receive.
    /// This is advertised to the remote connection in each packet we send.
    local_recieve_window: u32,
    /// Maximum number of bytes they can currently receive.
    /// We always want to keep current_window below this value.
    remote_receive_window: u32,
    /// Our lowest-ever seen timestamp difference in microseconds
    local_base_delay: u32,
    /// Their lowest-ever seen timestamp difference in microseconds
    remote_base_delay: u32,

    // TODO: Maybe make these maps of Instant -> delay sample
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
            // This is what libtorrent uses, which should let us send at least 1 packet to start
            remote_receive_window: ETHERNET_MTU,
            local_base_delay: u32::MAX,
            remote_base_delay: u32::MAX,
            local_delay_samples: vec![],
            remote_delay_samples: vec![],
        }
    }

    pub fn update_state(&mut self, received_at: u32, received_packet: &Packet) {
        let local_delay = received_at.wrapping_sub(received_packet.timestamp_micros);
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
