use std::net::SocketAddr;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("can't find socket address")]
    MissingAddress,
    #[error("too many connections. limit: {}", u16::MAX)]
    TooManyConnections,
    #[error("connection to {1} with id {0} already exists")]
    ConnectionExists(u16, SocketAddr),
    #[error(transparent)]
    // TODO: Could this be replaced with an io::Error with kind io::ErrorKind::InvalidData?
    PacketParseError(#[from] PacketParseError),
    #[error(transparent)]
    IOError(#[from] std::io::Error),
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
