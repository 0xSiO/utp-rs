use bytes::Bytes;

use crate::{
    packet::{Packet, PacketType},
    UtpListener, UtpSocket,
};

pub(crate) async fn get_socket() -> UtpSocket {
    UtpSocket::bind("localhost:0").await.unwrap()
}

pub(crate) fn get_packet() -> Packet {
    return Packet::new(PacketType::State, 1, 2, 3, 4, 5, 6, 7, vec![], Bytes::new());
}

pub(crate) async fn get_listener() -> UtpListener {
    UtpListener::bind("localhost:0").await.unwrap()
}
