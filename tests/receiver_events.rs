//! The events-channel responder of the vendored `airplay-client` must answer
//! every receiver request with a bare `200 OK`, tolerate fragmented and
//! multi-block messages, and never send anything volume related.
//!
//! The fake receiver below derives its keys independently from the pairing
//! secret (HKDF-SHA512, salt `Events-Salt`), so these tests also pin the key
//! directions: the sender encrypts with `Events-Read-Encryption-Key` and
//! decrypts with `Events-Write-Encryption-Key`.

use std::net::SocketAddr;
use std::time::Duration;

use airplay_client::receiver_events;
use airplay_crypto::chacha::ControlCipher;
use airplay_crypto::hkdf::derive_key_32;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const SECRET: &[u8] = &[0x42; 64];
const WAIT: Duration = Duration::from_secs(5);

fn key(secret: &[u8], info: &[u8]) -> [u8; 32] {
    derive_key_32(secret, b"Events-Salt", info).unwrap()
}

/// Receiver side cipher: it writes with `Events-Write` and reads `Events-Read`.
fn receiver_cipher(secret: &[u8]) -> ControlCipher {
    ControlCipher::new(
        key(secret, b"Events-Write-Encryption-Key"),
        key(secret, b"Events-Read-Encryption-Key"),
    )
}

/// Receiver using the opposite key roles (what the fallback has to handle).
fn swapped_receiver_cipher(secret: &[u8]) -> ControlCipher {
    ControlCipher::new(
        key(secret, b"Events-Read-Encryption-Key"),
        key(secret, b"Events-Write-Encryption-Key"),
    )
}

/// Starts the responder on the sender side and returns the receiver's socket.
async fn start(secret: &[u8]) -> (TcpStream, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    let sender = TcpStream::connect(address).await.unwrap();
    let (receiver, _) = listener.accept().await.unwrap();
    (receiver, receiver_events::spawn(sender, secret, address))
}

fn request(cseq: u32, uri: &str, body: &[u8]) -> Vec<u8> {
    let mut message = format!(
        "POST {uri} RTSP/1.0\r\nCSeq: {cseq}\r\nContent-Type: application/x-apple-binary-plist\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    message.extend_from_slice(body);
    message
}

async fn send(stream: &mut TcpStream, cipher: &mut ControlCipher, plaintext: &[u8]) {
    let encrypted = cipher.encrypt(plaintext).unwrap();
    stream.write_all(&encrypted).await.unwrap();
}

/// Reads one encrypted reply and checks it is a bare `200 OK`.
async fn read_reply(stream: &mut TcpStream, cipher: &mut ControlCipher) -> String {
    let mut plaintext = Vec::new();
    loop {
        let mut length = [0u8; 2];
        timeout(WAIT, stream.read_exact(&mut length))
            .await
            .expect("no reply in time")
            .unwrap();
        let block_len = u16::from_le_bytes(length);
        let mut block = vec![0u8; block_len as usize + 16];
        timeout(WAIT, stream.read_exact(&mut block))
            .await
            .unwrap()
            .unwrap();
        plaintext.extend(cipher.decrypt_block(&block, block_len).unwrap());
        if plaintext.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let reply = String::from_utf8(plaintext).unwrap();
    assert!(reply.starts_with("RTSP/1.0 200 OK\r\n"), "{reply:?}");
    assert!(reply.contains("Content-Length: 0\r\n"), "{reply:?}");
    assert!(
        !reply.to_ascii_lowercase().contains("volume"),
        "reply mentions volume: {reply:?}"
    );
    reply
}

async fn assert_no_data(stream: &mut TcpStream) {
    let mut byte = [0u8; 1];
    let result = timeout(Duration::from_millis(300), stream.read(&mut byte)).await;
    assert!(result.is_err(), "unexpected data from the responder");
}

#[tokio::test]
async fn single_request_gets_200_ok_echoing_cseq() {
    let (mut receiver, _task) = start(SECRET).await;
    let mut cipher = receiver_cipher(SECRET);

    send(
        &mut receiver,
        &mut cipher,
        &request(7, "/command", b"bplist00"),
    )
    .await;
    let reply = read_reply(&mut receiver, &mut cipher).await;
    assert!(reply.contains("CSeq: 7\r\n"), "{reply:?}");
    assert!(reply.contains("Server: AirTunes/"), "{reply:?}");
}

#[tokio::test]
async fn request_without_cseq_still_gets_a_reply() {
    let (mut receiver, _task) = start(SECRET).await;
    let mut cipher = receiver_cipher(SECRET);

    send(
        &mut receiver,
        &mut cipher,
        b"POST /command RTSP/1.0\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    let reply = read_reply(&mut receiver, &mut cipher).await;
    assert!(!reply.contains("CSeq"), "{reply:?}");
}

#[tokio::test]
async fn large_body_spanning_several_blocks_is_answered_once() {
    let (mut receiver, _task) = start(SECRET).await;
    let mut cipher = receiver_cipher(SECRET);

    let body: Vec<u8> = (0..3000u32).map(|n| (n % 251) as u8).collect();
    let message = request(1, "/command", &body);
    assert!(message.len() > 3 * 1024, "must need at least 3 blocks");
    send(&mut receiver, &mut cipher, &message).await;

    let reply = read_reply(&mut receiver, &mut cipher).await;
    assert!(reply.contains("CSeq: 1\r\n"), "{reply:?}");
    assert_no_data(&mut receiver).await;
}

#[tokio::test]
async fn byte_by_byte_fragmented_writes_are_reassembled() {
    let (mut receiver, _task) = start(SECRET).await;
    receiver.set_nodelay(true).unwrap();
    let mut cipher = receiver_cipher(SECRET);

    let encrypted = cipher
        .encrypt(&request(3, "/command", &[0xAB; 1500]))
        .unwrap();
    for byte in encrypted {
        receiver.write_all(&[byte]).await.unwrap();
        receiver.flush().await.unwrap();
    }
    let reply = read_reply(&mut receiver, &mut cipher).await;
    assert!(reply.contains("CSeq: 3\r\n"), "{reply:?}");
}

#[tokio::test]
async fn back_to_back_requests_keep_the_nonce_counters_in_step() {
    let (mut receiver, _task) = start(SECRET).await;
    let mut cipher = receiver_cipher(SECRET);

    // Two frames in one write, then two requests inside a single frame.
    let mut burst = cipher.encrypt(&request(1, "/a", b"")).unwrap();
    burst.extend(cipher.encrypt(&request(2, "/b", b"xy")).unwrap());
    receiver.write_all(&burst).await.unwrap();
    for expected in [1, 2] {
        let reply = read_reply(&mut receiver, &mut cipher).await;
        assert!(
            reply.contains(&format!("CSeq: {expected}\r\n")),
            "{reply:?}"
        );
    }

    let mut both = request(3, "/c", b"");
    both.extend(request(4, "/d", b""));
    send(&mut receiver, &mut cipher, &both).await;
    for expected in [3, 4] {
        let reply = read_reply(&mut receiver, &mut cipher).await;
        assert!(
            reply.contains(&format!("CSeq: {expected}\r\n")),
            "{reply:?}"
        );
    }
}

#[tokio::test]
async fn swapped_key_roles_are_detected_on_the_first_frame() {
    let (mut receiver, _task) = start(SECRET).await;
    let mut cipher = swapped_receiver_cipher(SECRET);

    for cseq in [1, 2] {
        send(&mut receiver, &mut cipher, &request(cseq, "/command", b"")).await;
        let reply = read_reply(&mut receiver, &mut cipher).await;
        assert!(reply.contains(&format!("CSeq: {cseq}\r\n")), "{reply:?}");
    }
}

#[tokio::test]
async fn unauthenticated_frame_ends_the_task_without_a_reply() {
    let (mut receiver, task) = start(SECRET).await;
    let mut cipher = receiver_cipher(&[0x13; 64]); // wrong shared secret

    send(&mut receiver, &mut cipher, &request(1, "/command", b"")).await;
    timeout(WAIT, task).await.expect("task must stop").unwrap();

    let mut rest = Vec::new();
    receiver.read_to_end(&mut rest).await.unwrap();
    assert!(
        rest.is_empty(),
        "nothing may be sent after a failed decrypt"
    );
}

#[tokio::test]
async fn receiver_close_ends_the_task() {
    let (receiver, task) = start(SECRET).await;
    drop(receiver);
    timeout(WAIT, task).await.expect("task must stop").unwrap();
}

#[tokio::test]
async fn receiver_responses_are_not_answered() {
    let (mut receiver, _task) = start(SECRET).await;
    let mut cipher = receiver_cipher(SECRET);

    send(
        &mut receiver,
        &mut cipher,
        b"RTSP/1.0 200 OK\r\nCSeq: 99\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    assert_no_data(&mut receiver).await;

    send(&mut receiver, &mut cipher, &request(5, "/command", b"")).await;
    let reply = read_reply(&mut receiver, &mut cipher).await;
    assert!(reply.contains("CSeq: 5\r\n"), "{reply:?}");
}

#[tokio::test]
async fn volume_related_requests_never_make_the_responder_mention_volume() {
    let (mut receiver, _task) = start(SECRET).await;
    let mut cipher = receiver_cipher(SECRET);

    let hostile = "POST /volume RTSP/1.0\r\nCSeq: 1 volume\r\nContent-Length: 29\r\n\r\nSET_PARAMETER volume: -20.000";
    send(&mut receiver, &mut cipher, hostile.as_bytes()).await;
    // CSeq is not a plain number: it is dropped, nothing from the request is echoed.
    let reply = read_reply(&mut receiver, &mut cipher).await;
    assert!(!reply.contains("CSeq"), "{reply:?}");

    send(
        &mut receiver,
        &mut cipher,
        &request(2, "/setVolume", b"{\"volume\":0.1}"),
    )
    .await;
    let reply = read_reply(&mut receiver, &mut cipher).await;
    assert!(reply.contains("CSeq: 2\r\n"), "{reply:?}");
}
