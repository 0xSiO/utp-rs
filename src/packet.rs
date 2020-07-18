use std::convert::TryFrom;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use num_enum::TryFromPrimitive;

use crate::error::*;

/// See http://bittorrent.org/beps/bep_0029.html#header-format
const PACKET_HEADER_LEN: usize = 20;

/// See http://bittorrent.org/beps/bep_0029.html#type
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, TryFromPrimitive)]
enum PacketType {
    Data = 0,
    Fin = 1,
    State = 2,
    Reset = 3,
    Syn = 4,
}

/// See http://bittorrent.org/beps/bep_0029.html#extension
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum ExtensionType {
    None,
    SelectiveAck,
    Bitfield,
    Unknown(u8),
}

impl From<u8> for ExtensionType {
    fn from(num: u8) -> Self {
        match num {
            0 => ExtensionType::None,
            1 => ExtensionType::SelectiveAck,
            2 => ExtensionType::Bitfield,
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
            ExtensionType::Unknown(n) => n,
        }
    }
}

/// See http://bittorrent.org/beps/bep_0029.html#extension
#[derive(Debug, PartialEq, Eq)]
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
#[derive(Debug, PartialEq, Eq)]
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
        let mut has_extensions = false;
        for extension in packet.extensions {
            has_extensions = true;
            result.put_u8(extension.extension_type.into());
            result.put_u8(extension.data.len() as u8);
            result.put(extension.data);
        }
        if has_extensions {
            // end extensions with a zero byte
            result.put_u8(0);
        }
        result.put(packet.data);
        result.freeze()
    }
}

// TODO: May want to support holding onto unknown extensions, doesn't seem like a good idea to
//       just throw away data
impl TryFrom<Bytes> for Packet {
    type Error = Error;

    fn try_from(mut bytes: Bytes) -> Result<Self> {
        if bytes.len() < PACKET_HEADER_LEN {
            return Err(PacketParseError::TooSmall.into());
        }

        let type_and_version = bytes.get_u8();
        let packet_type = PacketType::try_from(type_and_version >> 4)
            .map_err(|_| PacketParseError::InvalidType(type_and_version >> 4))?;

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

        // Consume the type if there's an actual extension
        if first_extension_type != ExtensionType::None.into() {
            if bytes.has_remaining() {
                // NOTE: The spec indicates that the first byte of an extension should be non-zero,
                //       as a zero byte terminates the list. In practice, however, a zero first
                //       byte in the extension list with a nonzero byte for the first extension
                //       type (given in the header) is just ignored.
                //
                // let actual_first_extension_type = bytes.get_u8();
                // if first_extension_type != actual_first_extension_type {
                //     return Err(PacketParseError::InvalidExtension(
                //         0,
                //         "extension type doesn't agree with advertised first extension type",
                //     )
                //     .into());
                // }
                bytes.advance(1);
            } else {
                return Err(PacketParseError::ExpectedExtension(0).into());
            }
        }

        let mut extension_type = first_extension_type;
        loop {
            match ExtensionType::from(extension_type) {
                ExtensionType::None => break,
                ExtensionType::SelectiveAck => {
                    // NOTE: The spec indicates that the length for a selective ack extension must
                    //       be at least 4, but in practice I've seen lengths of 2 or 3, so this
                    //       apparently isn't enforced. I'll at least require a length.
                    //
                    // if bytes.remaining() < 5 {
                    //     return Err(PacketParseError::InvalidExtension(
                    //         extension_number,
                    //         "selective ack extension needs at least 6 bytes: header (1), length (1), and bitfield (4)",
                    //     )
                    //     .into());
                    // }
                    if bytes.remaining() < 1 {
                        return Err(PacketParseError::ExtensionTooSmall {
                            index: extension_number,
                            expected: 2,
                            actual: 1,
                        }
                        .into());
                    }

                    let length = bytes.get_u8();

                    // NOTE: The spec indicates that the length should be in multiples of 4, but
                    //       like I noted earlier, this doesn't appear to be enforced.
                    //
                    // if length % 4 != 0 {
                    //     return Err(PacketParseError::InvalidExtension(
                    //         extension_number,
                    //         "selective ack requires length % 4 == 0",
                    //     )
                    //     .into());
                    // }

                    if bytes.remaining() < length as usize {
                        return Err(PacketParseError::ExtensionLengthTooLarge {
                            index: extension_number,
                            length,
                            remaining: bytes.remaining(),
                        }
                        .into());
                    }

                    let bitfield = bytes.split_to(length as usize);
                    extensions.push(Extension::new(ExtensionType::SelectiveAck, bitfield))
                }
                ExtensionType::Bitfield => {
                    // TODO: This is a deprecated extension
                }
                ExtensionType::Unknown(_) => {
                    // Unknown extension, just skip it
                    if bytes.remaining() < 1 {
                        return Err(PacketParseError::ExtensionTooSmall {
                            index: extension_number,
                            expected: 2,
                            actual: 1,
                        }
                        .into());
                    }

                    let length = bytes.get_u8();
                    if bytes.remaining() < length as usize {
                        return Err(PacketParseError::ExtensionLengthTooLarge {
                            index: extension_number,
                            length,
                            remaining: bytes.remaining(),
                        }
                        .into());
                    }
                    let _ = bytes.split_to(length as usize);
                }
            }
            extension_number += 1;
            if bytes.has_remaining() {
                extension_type = bytes.get_u8();
            } else {
                return Err(PacketParseError::ExpectedExtension(extension_number).into());
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
                 0x01, 0x04, 0x00, 0x01, 0x00, 0x01,
                 // end extensions
                 0x00]
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
                 // end extensions
                 0x00,
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
                 0x01, 0x04, 0x00, 0x01, 0x01, 0x00,
                 // end extensions
                 0x00]
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
                  0x01, 0x04, 0x00, 0x01, 0x00, 0x01,
                  // end extensions
                  0x00])).unwrap(),
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
                  0xff, 0x03, 0x00, 0x01, 0x00,
                  // end extensions
                  0x00])).unwrap(),
            new_packet(vec![], Bytes::new())
        );
    }

    #[test]
    fn from_non_conforming_bytes_with_extension_test() {
        // This tests behavior that doesn't conform to the protocol spec

        // This is ok because we skip the type of the first extension in the list
        #[rustfmt::skip]
        assert!(
            Packet::try_from(Bytes::from_static(
                // say extension type is 1, but give extension type 2
                &[0x02 << 4 | 0x01, 0x01, 0x30, 0x39,
                  0x00, 0x03, 0xc4, 0x1a,
                  0x00, 0x00, 0x00, 0x28,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00,
                  0x02, 0x00, 0x00])).is_ok()
        );

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
                  0x01, 0x01, 0xff, 0x00])).is_ok()
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
                  0xff, 0x02, 0x00])).is_err()
        );

        #[rustfmt::skip]
        assert!(
            Packet::try_from(Bytes::from_static(
                // missing a 0x00 at end of extension list
                &[0x02 << 4 | 0x01, 0xff, 0x30, 0x39,
                  0x00, 0x03, 0xc4, 0x1a,
                  0x00, 0x00, 0x00, 0x28,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00,
                  0xff, 0x01, 0x00])).is_err()
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
                &[0x02 << 4 | 0x01, 0x01, 0x30, 0x39,
                  0x00, 0x03, 0xc4, 0x1a,
                  0x00, 0x00, 0x00, 0x28,
                  0x00, 0x00, 0x10, 0x00,
                  0x00, 0x00, 0x00, 0x00,
                  // selective ack extension with bitfield
                  0x01, 0x04, 0x00, 0x01, 0x00, 0x01,
                  // end extensions
                  0x00,
                  // data
                  0x01, 0x02, 0x03, 0x04, 0x05])).unwrap(),
            new_packet(
                vec![Extension::new(
                    ExtensionType::SelectiveAck,
                    Bytes::from_static(&[0x00, 0x01, 0x00, 0x01]),
                )],
                Bytes::from_static(&[0x01, 0x02, 0x03, 0x04, 0x05]),
            )
        );
    }
}
