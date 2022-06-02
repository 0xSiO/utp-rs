use std::time::{SystemTime, UNIX_EPOCH};

/// Get the last 32 bits of the current UNIX timestamp in microseconds.
///
/// We only care about the last 32 bits because u32::MAX microseconds is about 72 minutes, which is
/// plenty of time to measure packet transmission delays.
pub fn current_micros() -> u32 {
    let now = SystemTime::now();
    let total_microseconds = now.duration_since(UNIX_EPOCH).unwrap().as_micros();
    (total_microseconds & u32::MAX as u128) as u32
}
