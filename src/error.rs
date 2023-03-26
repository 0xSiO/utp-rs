use std::net::SocketAddr;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error("no socket address provided")]
    NoAddress,
    #[error("too many connections. limit: {}", u16::MAX)]
    TooMany,
    #[error("connection to {1} with id {0} already exists")]
    AlreadyExists(u16, SocketAddr),
}

#[derive(Debug, Error)]
pub enum PacketParseError {
    #[error("packet too small, must be at least 20 bytes")]
    TooSmall,
    #[error("unsupported packet version: {0}")]
    UnsupportedVersion(u8),
    #[error("invalid packet type: {0}")]
    InvalidPacketType(u8),
    #[error("expected extension {0}, but hit end of buffer")]
    MissingExtension(usize),
    #[error(
        "extension {index}'s length ({length}) exceeds number of remaining bytes ({remaining})'"
    )]
    IncompleteExtension {
        index: usize,
        length: usize,
        remaining: usize,
    },
}
