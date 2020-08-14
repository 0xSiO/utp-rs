use utp::{Packet, UtpListener};

#[tokio::test]
async fn utp_listener() {
    let listener = UtpListener::bind("127.0.0.1:4000").await.unwrap();
    // let handle = tokio::spawn(listener.listen());
}
