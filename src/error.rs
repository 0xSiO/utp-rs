use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid packet type: {0}")]
    InvalidPacketType(u8),
    #[error(transparent)]
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
    #[error("expected extension {0}, but hit end of buffer")]
    ExpectedExtension(usize),
    #[error("extension {index} requires at least {expected} bytes, found {actual}")]
    ExtensionTooSmall {
        index: usize,
        expected: u8,
        actual: u8,
    },
    #[error(
        "extension {index}'s length ({length}) exceeds number of remaining bytes ({remaining})'"
    )]
    ExtensionLengthTooLarge {
        index: usize,
        length: u8,
        remaining: usize,
    },
}
