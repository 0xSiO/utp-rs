use std::net::SocketAddr;

use tokio::net::{lookup_host, ToSocketAddrs};

use crate::error::*;

pub(crate) async fn resolve(addr: impl ToSocketAddrs) -> Result<SocketAddr> {
    lookup_host(addr)
        .await?
        .next()
        .ok_or_else(|| Error::MissingAddress)
}
