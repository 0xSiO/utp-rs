use std::{fmt, net::SocketAddr, sync::Arc};

use log::debug;
use tokio::net::{lookup_host, ToSocketAddrs};

use crate::{error::*, socket::UtpSocket};

// TODO: Need to figure out a plan to deal with lost packets: one idea is to have a queue
// of unacked packets, pass a reference into the write future, and access the queue from
// the future... something like that
pub struct UtpStream {
    socket: Arc<UtpSocket>,
    connection_id: u16,
    remote_addr: SocketAddr,
    // TODO: Queued writes?
}

impl UtpStream {
    pub(crate) fn new(socket: Arc<UtpSocket>, connection_id: u16, remote_addr: SocketAddr) -> Self {
        Self {
            socket,
            connection_id,
            remote_addr,
        }
    }

    pub async fn connect(socket: Arc<UtpSocket>, remote_addr: impl ToSocketAddrs) -> Result<Self> {
        let remote_addr = lookup_host(remote_addr)
            .await?
            .next()
            .ok_or_else(|| Error::MissingAddress)?;
        let connection_id = socket.register_connection(remote_addr).await?;
        Ok(Self::new(socket, connection_id, remote_addr))
    }

    pub fn connection_id(&self) -> u16 {
        self.connection_id
    }

    pub async fn recv(&self) -> Result<()> {
        let packet = self
            .socket
            .get_packet(self.connection_id, self.remote_addr)
            .await?;
        debug!(
            "connection {} received {:?} from {}",
            self.connection_id, packet.packet_type, self.remote_addr
        );
        // TODO: Add packet data to some kind of internal buffer
        Ok(())
    }
}

impl fmt::Debug for UtpStream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_fmt(format_args!(
            "UtpStream {{ id: {}, local_addr: {}, remote_addr: {} }}",
            self.connection_id,
            self.socket.local_addr(),
            self.remote_addr
        ))
    }
}
