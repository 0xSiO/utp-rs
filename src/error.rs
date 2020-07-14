use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    PacketParseError(#[from] PacketParseError),
    #[error(transparent)]
    IOError(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum PacketParseError {
    #[error("packet too small, must be at least 20 bytes")]
    TooSmall,
    #[error("invalid packet type: {0}")]
    InvalidType(u8),
    #[error("unsupported packet version: {0}")]
    UnsupportedVersion(u8),
    #[error("packet extension {0} is invalid: {1}")]
    InvalidExtension(usize, &'static str),
    #[error("expected extension {0}, but hit end of buffer")]
    ExpectedExtension(usize),
}
