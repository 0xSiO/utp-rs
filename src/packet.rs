use std::convert::TryFrom;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use num_enum::{IntoPrimitive, TryFromPrimitive};

use crate::error::*;

/// See http://bittorrent.org/beps/bep_0029.html#header-format
const PACKET_HEADER_LEN: usize = 20;

/// See http://bittorrent.org/beps/bep_0029.html#type
#[repr(u8)]
#[derive(Debug, Copy, Clone, IntoPrimitive, TryFromPrimitive)]
enum PacketType {
    Data = 0,
    Fin = 1,
    State = 2,
    Reset = 3,
    Syn = 4,
}

/// See http://bittorrent.org/beps/bep_0029.html#extension
#[derive(Debug, Copy, Clone, IntoPrimitive, TryFromPrimitive)]
#[repr(u8)]
enum ExtensionType {
    None = 0,
    SelectiveAck = 1,
}

/// See http://bittorrent.org/beps/bep_0029.html#extension
#[derive(Debug)]
struct Extension {
    extension_type: ExtensionType,
    data: Bytes,
}

impl Extension {
    pub fn new(extension_type: ExtensionType, data: Bytes) -> Self {
        Self {
            extension_type,
            data,
        }
    }
}

/// See http://bittorrent.org/beps/bep_0029.html#header-format
#[derive(Debug)]
pub struct Packet {
    packet_type: PacketType,
    version: u8,
    connection_id: u16,
    timestamp_micros: u32,
    timestamp_delta_micros: u32,
    window_size: u32,
    seq_number: u16,
    ack_number: u16,
    extensions: Vec<Extension>,
    data: Bytes,
}

impl Packet {
    fn new(
        packet_type: PacketType,
        version: u8,
        connection_id: u16,
        timestamp_micros: u32,
        timestamp_delta_micros: u32,
        window_size: u32,
        seq_number: u16,
        ack_number: u16,
        extensions: Vec<Extension>,
        data: Bytes,
    ) -> Self {
        Self {
            packet_type,
            version,
            connection_id,
            timestamp_micros,
            timestamp_delta_micros,
            window_size,
            seq_number,
            ack_number,
            extensions,
            data,
        }
    }
}

impl From<Packet> for Bytes {
    fn from(packet: Packet) -> Self {
        let mut packet_length = PACKET_HEADER_LEN + packet.data.len();
        for extension in packet.extensions.iter() {
            // Have to account for type + length + data
            packet_length += 1 + 1 + extension.data.len();
        }

        let mut result = BytesMut::with_capacity(packet_length);
        result.put_u8((packet.packet_type as u8) << 4 | packet.version);
        if packet.extensions.is_empty() {
            result.put_u8(ExtensionType::None.into());
        } else {
            result.put_u8(packet.extensions[0].extension_type.into());
        }
        result.put_u16(packet.connection_id);
        result.put_u32(packet.timestamp_micros);
        result.put_u32(packet.timestamp_delta_micros);
        result.put_u32(packet.window_size);
        result.put_u16(packet.seq_number);
        result.put_u16(packet.ack_number);
        // TODO: Do we need to think about padding?
        for extension in packet.extensions {
            result.put_u8(extension.extension_type.into());
            result.put_u8(extension.data.len() as u8);
            result.put(extension.data);
        }
        result.put(packet.data);
        result.freeze()
    }
}

impl TryFrom<Bytes> for Packet {
    type Error = Error;

    fn try_from(mut bytes: Bytes) -> Result<Self> {
        if bytes.len() < PACKET_HEADER_LEN {
            return Err(PacketParseError::TooSmall.into());
        }

        let type_and_version = bytes.get_u8();
        let packet_type = match type_and_version >> 4 {
            t if t == PacketType::Data as u8 => PacketType::Data,
            t if t == PacketType::Fin as u8 => PacketType::Fin,
            t if t == PacketType::State as u8 => PacketType::State,
            t if t == PacketType::Reset as u8 => PacketType::Reset,
            t if t == PacketType::Syn as u8 => PacketType::Syn,
            t => return Err(PacketParseError::InvalidType(t).into()),
        };

        let version = match type_and_version & 0x0F {
            1 => 1,
            v => return Err(PacketParseError::UnsupportedVersion(v).into()),
        };

        let first_extension_type = bytes.get_u8();
        let connection_id = bytes.get_u16();
        let timestamp_micros = bytes.get_u32();
        let timestamp_delta_micros = bytes.get_u32();
        let window_size = bytes.get_u32();
        let seq_number = bytes.get_u16();
        let ack_number = bytes.get_u16();

        // End of packet header, now we check for extensions
        let mut extensions = vec![];
        let mut extension_number = 0;
        let mut extension_type = first_extension_type;
        loop {
            match ExtensionType::try_from(extension_type) {
                Ok(ExtensionType::None) => break,
                Ok(ExtensionType::SelectiveAck) => {
                    if bytes.remaining() < 6 {
                        return Err(PacketParseError::InvalidExtension(
                            extension_number,
                            "selective ack extension needs at least 6 bytes",
                        )
                        .into());
                    }

                    bytes.advance(1);
                    let length = bytes.get_u8();

                    if length % 4 != 0 {
                        return Err(PacketParseError::InvalidExtension(
                            extension_number,
                            "selective ack requires length % 4 == 0",
                        )
                        .into());
                    }
                    if bytes.remaining() < length as usize {
                        return Err(PacketParseError::InvalidExtension(
                            extension_number,
                            "length exceeds number of remaining bytes",
                        )
                        .into());
                    }

                    let bitfield = bytes.split_to(length as usize);
                    extensions.push(Extension::new(ExtensionType::SelectiveAck, bitfield))
                }
                Err(_) => {
                    // Unknown extension, just skip it
                    if bytes.remaining() < 2 {
                        return Err(PacketParseError::InvalidExtension(
                            extension_number,
                            "extensions require at least 2 bytes",
                        )
                        .into());
                    }

                    bytes.advance(1);
                    let length = bytes.get_u8();
                    if bytes.remaining() < length as usize {
                        return Err(PacketParseError::InvalidExtension(
                            extension_number,
                            "length exceeds number of remaining bytes",
                        )
                        .into());
                    }
                    let _ = bytes.split_to(length as usize);
                }
            }
            extension_number += 1;
            if bytes.has_remaining() {
                extension_type = bytes.get_u8();
            } else {
                return Err(PacketParseError::InvalidExtension(
                    extension_number,
                    "expected extension, but hit end of buffer",
                )
                .into());
            }
        }

        Ok(Self::new(
            packet_type,
            version,
            connection_id,
            timestamp_micros,
            timestamp_delta_micros,
            window_size,
            seq_number,
            ack_number,
            extensions,
            bytes,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_packet(extensions: Vec<Extension>, data: Bytes) -> Packet {
        Packet::new(
            PacketType::State,
            1,
            12345,
            246810,
            40,
            4096,
            0,
            0,
            extensions,
            data,
        )
    }

    #[test]
    fn into_bytes_test() {
        let packet = new_packet(vec![], Bytes::new());
        #[rustfmt::skip]
        assert_eq!(
            Bytes::from(packet).to_vec(),
            vec![0x02 << 4 | 0x01, 0x00, 0x30, 0x39,
                 0x00, 0x03, 0xc4, 0x1a,
                 0x00, 0x00, 0x00, 0x28,
                 0x00, 0x00, 0x10, 0x00,
                 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn into_bytes_with_extension_test() {
        let packet = new_packet(
            vec![Extension::new(
                ExtensionType::SelectiveAck,
                Bytes::from_static(&[0x00, 0x01, 0x00, 0x01]),
            )],
            Bytes::new(),
        );
        #[rustfmt::skip]
        assert_eq!(
            Bytes::from(packet).to_vec(),
            vec![0x02 << 4 | 0x01, 0x01, 0x30, 0x39,
                 0x00, 0x03, 0xc4, 0x1a,
                 0x00, 0x00, 0x00, 0x28,
                 0x00, 0x00, 0x10, 0x00,
                 0x00, 0x00, 0x00, 0x00,
                 // selective ack extension with bitfield
                 0x01, 0x04, 0x00, 0x01, 0x00, 0x01]
        );
    }

    #[test]
    fn into_bytes_with_data_test() {
        let packet = new_packet(vec![], Bytes::from_static(&[0x01, 0x02, 0x03, 0x04, 0x05]));
        #[rustfmt::skip]
        assert_eq!(
            Bytes::from(packet).to_vec(),
            vec![0x02 << 4 | 0x01, 0x00, 0x30, 0x39,
                 0x00, 0x03, 0xc4, 0x1a,
                 0x00, 0x00, 0x00, 0x28,
                 0x00, 0x00, 0x10, 0x00,
                 0x00, 0x00, 0x00, 0x00,
                 // data
                 0x01, 0x02, 0x03, 0x04, 0x05]
        );
    }

    #[test]
    fn into_bytes_with_extension_and_data_test() {
        let packet = new_packet(
            vec![Extension::new(
                ExtensionType::SelectiveAck,
                Bytes::from_static(&[0x00, 0x01, 0x00, 0x01]),
            )],
            Bytes::from_static(&[0x01, 0x02, 0x03, 0x04, 0x05]),
        );
        #[rustfmt::skip]
        assert_eq!(
            Bytes::from(packet).to_vec(),
            vec![0x02 << 4 | 0x01, 0x01, 0x30, 0x39,
                 0x00, 0x03, 0xc4, 0x1a,
                 0x00, 0x00, 0x00, 0x28,
                 0x00, 0x00, 0x10, 0x00,
                 0x00, 0x00, 0x00, 0x00,
                 // selective ack extension with bitfield
                 0x01, 0x04, 0x00, 0x01, 0x00, 0x01,
                 // data
                 0x01, 0x02, 0x03, 0x04, 0x05]
        );
    }

    #[test]
    fn multiple_extensions_test() {
        let packet = new_packet(
            vec![
                Extension::new(
                    ExtensionType::SelectiveAck,
                    Bytes::from_static(&[0x00, 0x01, 0x00, 0x01]),
                ),
                Extension::new(
                    ExtensionType::SelectiveAck,
                    Bytes::from_static(&[0x01, 0x00, 0x00, 0x01]),
                ),
                Extension::new(
                    ExtensionType::SelectiveAck,
                    Bytes::from_static(&[0x00, 0x01, 0x01, 0x00]),
                ),
            ],
            Bytes::new(),
        );
        #[rustfmt::skip]
        assert_eq!(
            Bytes::from(packet).to_vec(),
            vec![0x02 << 4 | 0x01, 0x01, 0x30, 0x39,
                 0x00, 0x03, 0xc4, 0x1a,
                 0x00, 0x00, 0x00, 0x28,
                 0x00, 0x00, 0x10, 0x00,
                 0x00, 0x00, 0x00, 0x00,
                 // 3 extension segments
                 0x01, 0x04, 0x00, 0x01, 0x00, 0x01,
                 0x01, 0x04, 0x01, 0x00, 0x00, 0x01,
                 0x01, 0x04, 0x00, 0x01, 0x01, 0x00]
        );
    }
}
