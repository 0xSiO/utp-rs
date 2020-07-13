use bytes::{BufMut, Bytes, BytesMut};

/// See http://bittorrent.org/beps/bep_0029.html#header-format
const PACKET_HEADER_LEN: usize = 128;

/// See http://bittorrent.org/beps/bep_0029.html#type
#[repr(u8)]
#[derive(Debug)]
enum PacketType {
    Data = 0,
    Fin = 1,
    State = 2,
    Reset = 3,
    Syn = 4,
}

/// See http://bittorrent.org/beps/bep_0029.html#header-format
#[derive(Debug)]
struct Packet {
    packet_type: PacketType,
    version: u8,
    extension: u8,
    connection_id: u16,
    timestamp_micros: u32,
    timestamp_delta_micros: u32,
    window_size: u32,
    seq_number: u16,
    ack_number: u16,
    data: Bytes,
}

impl Packet {
    pub fn new(
        packet_type: PacketType,
        version: u8,
        extension: u8,
        connection_id: u16,
        timestamp_micros: u32,
        timestamp_delta_micros: u32,
        window_size: u32,
        seq_number: u16,
        ack_number: u16,
        data: Bytes,
    ) -> Self {
        Self {
            packet_type,
            version,
            extension,
            connection_id,
            timestamp_micros,
            timestamp_delta_micros,
            window_size,
            seq_number,
            ack_number,
            data,
        }
    }
}

impl From<Packet> for Bytes {
    fn from(packet: Packet) -> Self {
        let mut result = BytesMut::with_capacity(PACKET_HEADER_LEN + packet.data.len());
        result.put_u8((packet.packet_type as u8) << 4 | packet.version);
        result.put_u8(packet.extension);
        result.put_u16(packet.connection_id);
        result.put_u32(packet.timestamp_micros);
        result.put_u32(packet.timestamp_delta_micros);
        result.put_u32(packet.window_size);
        result.put_u16(packet.seq_number);
        result.put_u16(packet.ack_number);
        // TODO: Extension + data bytes. What sort of padding do we need?
        result.freeze()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_packet() -> Packet {
        Packet::new(PacketType::State, 1, 0, 0, 0, 0, 0, 0, 0, Bytes::new())
    }

    #[test]
    fn into_bytes_test() {
        let packet = new_packet();
        // TODO: assert_eq!(Bytes::from(packet).to_vec(), vec![]);
    }
}
