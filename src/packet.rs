use std::convert::TryFrom;

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::error::*;

/// See https://www.bittorrent.org/beps/bep_0029.html#header-format
pub(crate) const PACKET_HEADER_LEN: usize = 20;

/// See https://www.bittorrent.org/beps/bep_0029.html#type
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum PacketType {
    Data = 0,
    Fin = 1,
    State = 2,
    Reset = 3,
    Syn = 4,
}

impl TryFrom<u8> for PacketType {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(PacketType::Data),
            1 => Ok(PacketType::Fin),
            2 => Ok(PacketType::State),
            3 => Ok(PacketType::Reset),
            4 => Ok(PacketType::Syn),
            n => Err(PacketParseError::InvalidPacketType(n).into()),
        }
    }
}

/// See https://www.bittorrent.org/beps/bep_0029.html#extension and UTP-related code in
/// https://github.com/arvidn/libtorrent
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum ExtensionType {
    None,
    SelectiveAck,
    Bitfield,    // TODO: This type is deprecated
    CloseReason, // See include/libtorrent/close_reason.hpp in libtorrent
    Unknown(u8),
}

impl From<u8> for ExtensionType {
    fn from(num: u8) -> Self {
        match num {
            0 => ExtensionType::None,
            1 => ExtensionType::SelectiveAck,
            2 => ExtensionType::Bitfield,
            3 => ExtensionType::CloseReason,
            n => ExtensionType::Unknown(n),
        }
    }
}

impl From<ExtensionType> for u8 {
    fn from(extension_type: ExtensionType) -> Self {
        match extension_type {
            ExtensionType::None => 0,
            ExtensionType::SelectiveAck => 1,
            ExtensionType::Bitfield => 2,
            ExtensionType::CloseReason => 3,
            ExtensionType::Unknown(n) => n,
        }
    }
}

/// See https://www.bittorrent.org/beps/bep_0029.html#extension
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Extension {
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

/// See https://www.bittorrent.org/beps/bep_0029.html#header-format
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub(crate) packet_type: PacketType,
    pub(crate) version: u8,
    pub(crate) connection_id: u16,
    pub(crate) timestamp_micros: u32,
    pub(crate) timestamp_delta_micros: u32,
    pub(crate) window_size: u32,
    pub(crate) seq_number: u16,
    pub(crate) ack_number: u16,
    pub(crate) extensions: Vec<Extension>,
    pub(crate) data: Bytes,
}

impl Packet {
    pub(crate) fn new(
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

        // Linked list of packet extensions. Each extension header contains the type byte
        // of the next extension in the list, or 0 if there are no more extensions.
        for i in 0..packet.extensions.len() {
            result.put_u8(
                packet
                    .extensions
                    .get(i + 1)
                    .map_or(0, |e| e.extension_type.into()),
            );
            let extension = &packet.extensions[i];
            result.put_u8(extension.data.len() as u8);
            result.put(extension.data.as_ref());
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
        let packet_type = PacketType::try_from(type_and_version >> 4)?;

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

        // Store the type byte of the next extension in the list. If this is zero, that
        // means the current extension is the last one in the list.
        let mut next_extension_type = if extension_type != 0 {
            if bytes.has_remaining() {
                bytes.get_u8()
            } else {
                return Err(PacketParseError::MissingExtension(0).into());
            }
        } else {
            0
        };

        loop {
            match ExtensionType::from(extension_type) {
                ExtensionType::None => break,
                other_type => {
                    // NOTE: The spec indicates that the length byte for a selective ack extension
                    // must be at least 4 and in multiples of 4, but in practice I've seen lengths
                    // of 2 or 3, so this apparently isn't enforced.

                    if !bytes.has_remaining() {
                        return Err(PacketParseError::MissingExtension(extension_number).into());
                    }

                    let length = bytes.get_u8() as usize;

                    if length > bytes.remaining() {
                        return Err(PacketParseError::IncompleteExtension {
                            index: extension_number,
                            length,
                            remaining: bytes.remaining(),
                        }
                        .into());
                    }

                    let extension_data = bytes.split_to(length);
                    extensions.push(Extension::new(other_type, extension_data));
                }
            }

            extension_number += 1;
            extension_type = next_extension_type;

            // Don't consume the next byte unless we're expecting another extension
            if next_extension_type != 0 && bytes.has_remaining() {
                next_extension_type = bytes.get_u8();
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
                 0x00, 0x04, 0x00, 0x01, 0x00, 0x01]
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
                 0x00, 0x04, 0x00, 0x01, 0x00, 0x01,
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
                    ExtensionType::Bitfield,
                    Bytes::from_static(&[0x01, 0x00, 0x00, 0x01]),
                ),
                Extension::new(
                    ExtensionType::CloseReason,
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
                 0x02, 0x04, 0x00, 0x01, 0x00, 0x01,
                 0x03, 0x04, 0x01, 0x00, 0x00, 0x01,
                 0x00, 0x04, 0x00, 0x01, 0x01, 0x00]
        );
    }

    #[test]
    fn from_bytes_test() {
        #[rustfmt::skip]
        assert_eq!(
            Packet::try_from(Bytes::from_static(
                &[0x02 << 4 | 0x01, 0x00, 0x30, 0x39,
                  0x00, 0x03, 0xc4, 0x1a,
                  0x00, 0x00, 0x00, 0x28,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00])).unwrap(),
            new_packet(vec![], Bytes::new())
        );
    }

    #[test]
    fn from_malformed_bytes_test() {
        #[rustfmt::skip]
        assert!(
            Packet::try_from(Bytes::from_static(
                // invalid length
                &[0x02 << 4 | 0x01, 0x00, 0x30, 0x39,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00])).is_err()
        );

        #[rustfmt::skip]
        assert!(
            Packet::try_from(Bytes::from_static(
                // invalid type
                &[0xff << 4 | 0x01, 0x00, 0x30, 0x39,
                  0x00, 0x03, 0xc4, 0x1a,
                  0x00, 0x00, 0x00, 0x28,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00])).is_err()
        );

        #[rustfmt::skip]
        assert!(
            Packet::try_from(Bytes::from_static(
                // invalid version
                &[0x02 << 4 | 0xff, 0x00, 0x30, 0x39,
                  0x00, 0x03, 0xc4, 0x1a,
                  0x00, 0x00, 0x00, 0x28,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00])).is_err()
        );

        #[rustfmt::skip]
        assert!(
            Packet::try_from(Bytes::from_static(
                // give extension type, but no extension
                &[0x02 << 4 | 0x01, 0x01, 0x30, 0x39,
                  0x00, 0x03, 0xc4, 0x1a,
                  0x00, 0x00, 0x00, 0x28,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00])).is_err()
        );
    }

    #[test]
    fn from_bytes_with_extension_test() {
        #[rustfmt::skip]
        assert_eq!(
            Packet::try_from(Bytes::from_static(
                &[0x02 << 4 | 0x01, 0x01, 0x30, 0x39,
                  0x00, 0x03, 0xc4, 0x1a,
                  0x00, 0x00, 0x00, 0x28,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00,
                  // selective ack extension with bitfield
                  0x00, 0x04, 0x00, 0x01, 0x00, 0x01])).unwrap(),
            new_packet(
                vec![Extension::new(
                    ExtensionType::SelectiveAck,
                    Bytes::from_static(&[0x00, 0x01, 0x00, 0x01]),
                )],
                Bytes::new(),
            )
        );
    }

    #[test]
    fn from_bytes_with_unknown_extension_test() {
        #[rustfmt::skip]
        assert_eq!(
            Packet::try_from(Bytes::from_static(
                &[0x02 << 4 | 0x01, 0xff, 0x30, 0x39,
                  0x00, 0x03, 0xc4, 0x1a,
                  0x00, 0x00, 0x00, 0x28,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00,
                  // made-up extension with length 3
                  0x00, 0x03, 0x00, 0x01, 0x00])).unwrap(),
            new_packet(
                vec![Extension::new(
                    ExtensionType::Unknown(0xff),
                    Bytes::from_static(&[0x00, 0x01, 0x00]),
                )],
                Bytes::new(),
            )
        );
    }

    #[test]
    fn from_non_conforming_bytes_with_extension_test() {
        // This tests behavior that doesn't conform to the protocol spec

        // This is ok because the length % 4 == 0 check is not enforced in practice
        #[rustfmt::skip]
        assert!(
            Packet::try_from(Bytes::from_static(
                // invalid selective ack extension, according to spec
                &[0x02 << 4 | 0x01, 0x01, 0x30, 0x39,
                  0x00, 0x03, 0xc4, 0x1a,
                  0x00, 0x00, 0x00, 0x28,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00,
                  // length is 1 rather than minimum of 4
                  0x00, 0x01, 0xff])).is_ok()
        );
    }

    #[test]
    fn from_malformed_bytes_with_extension_test() {
        #[rustfmt::skip]
        assert!(
            Packet::try_from(Bytes::from_static(
                // invalid extension length
                &[0x02 << 4 | 0x01, 0xff, 0x30, 0x39,
                  0x00, 0x03, 0xc4, 0x1a,
                  0x00, 0x00, 0x00, 0x28,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00,
                  0x00, 0x02, 0xab])).is_err()
        );

        #[rustfmt::skip]
        assert!(
            Packet::try_from(Bytes::from_static(
                // missing an extension
                &[0x02 << 4 | 0x01, 0xff, 0x30, 0x39,
                  0x00, 0x03, 0xc4, 0x1a,
                  0x00, 0x00, 0x00, 0x28,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00,
                  0x02, 0x01, 0x00])).is_err()
        );
    }

    #[test]
    fn from_bytes_with_data_test() {
        #[rustfmt::skip]
        assert_eq!(
            Packet::try_from(Bytes::from_static(
                &[0x02 << 4 | 0x01, 0x00, 0x30, 0x39,
                  0x00, 0x03, 0xc4, 0x1a,
                  0x00, 0x00, 0x00, 0x28,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00,
                  // data
                  0x01, 0x02, 0x03, 0x04, 0x05])).unwrap(),
            new_packet(
                vec![],
                Bytes::from_static(&[0x01, 0x02, 0x03, 0x04, 0x05]),
            )
        );
    }

    #[test]
    fn from_bytes_with_extension_and_data_test() {
        #[rustfmt::skip]
        assert_eq!(
            Packet::try_from(Bytes::from_static(
                &[0x02 << 4 | 0x01, 0x03, 0x30, 0x39,
                  0x00, 0x03, 0xc4, 0x1a,
                  0x00, 0x00, 0x00, 0x28,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00,
                  // 'close reason' extension with random data
                  0x00, 0x04, 0x00, 0x01, 0x00, 0x01,
                  // data
                  0x01, 0x02, 0x03, 0x04, 0x05])).unwrap(),
            new_packet(
                vec![Extension::new(
                    ExtensionType::CloseReason,
                    Bytes::from_static(&[0x00, 0x01, 0x00, 0x01]),
                )],
                Bytes::from_static(&[0x01, 0x02, 0x03, 0x04, 0x05]),
            )
        );
    }
}
