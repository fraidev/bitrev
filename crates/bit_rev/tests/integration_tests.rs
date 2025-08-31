use bit_rev::protocol::Protocol;
use std::io::Error;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind to a port");
    let local_addr = listener.local_addr().expect("Failed to get local address");
    local_addr.port()
}

async fn handler(server_handshake: &[u8], bitfield: &[u8], port: u16) -> Result<(), Error> {
    let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).await?;

    let (mut socket, _) = listener.accept().await?;

    // Handshake handling
    let mut handshake = vec![0u8; 68];
    socket.read_exact(&mut handshake).await?;
    socket.write_all(server_handshake).await?;

    // Send bitfield
    socket.write_all(bitfield).await
}

#[tokio::test]
async fn successful_handshake_test() {
    println!("Running successful_handshake_test");
    let port = find_available_port().await;
    let server_handshake = vec![
        19, 66, 105, 116, 84, 111, 114, 114, 101, 110, 116, 32, 112, 114, 111, 116, 111, 99, 111,
        108, 0, 0, 0, 0, 0, 0, 0, 0, 134, 212, 200, 0, 36, 164, 105, 190, 76, 80, 188, 90, 16, 44,
        247, 23, 128, 49, 0, 116, 45, 83, 89, 48, 48, 49, 48, 45, 192, 125, 147, 203, 136, 32, 59,
        180, 253, 168, 193, 19,
    ];

    let bitfield = vec![0, 0, 0, 3, 5, 0b01010100, 0b01010100];

    let info_hash = vec![
        134, 212, 200, 0, 36, 164, 105, 190, 76, 80, 188, 90, 16, 44, 247, 23, 128, 49, 0, 116,
    ]
    .as_slice()
    .try_into()
    .unwrap();
    let client_peer_id = vec![
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
    ]
    .as_slice()
    .try_into()
    .unwrap();

    let server_handshake_clone = server_handshake.clone();
    let bitfield_clone = bitfield.clone();
    tokio::spawn(async move {
        handler(&server_handshake_clone, &bitfield_clone, port)
            .await
            .expect("Failed to handle handshake");
    });

    // Wait for the server to start
    tokio::time::sleep(Duration::from_millis(200)).await;
    let peer_addr = SocketAddr::new(std::net::Ipv4Addr::new(127, 0, 0, 1).into(), port);
    let protocol = Protocol::connect(peer_addr, info_hash, client_peer_id)
        .await
        .expect("Failed to connect");

    // assert!(protocol.choked);
    assert_eq!(protocol.peer_id, client_peer_id);
    assert_eq!(protocol.info_hash, info_hash);
    assert_eq!(protocol.peer, peer_addr);
}
