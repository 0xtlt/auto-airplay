//! Responder for the receiver's requests on the AirPlay 2 "events" channel.
//!
//! After SETUP phase 1 the sender opens a TCP connection to the receiver's
//! `eventPort`. The receiver uses it as a reverse channel: it sends
//! HomeKit-encrypted RTSP-style requests (for example `POST /command`) and
//! expects every one of them to be answered. A sender that never reads the
//! channel can have the receiver close the control connection some time later.
//!
//! This task only reads the frames, parses the request head and answers
//! `200 OK`. It never looks into a request body and never acts on a request:
//! in particular it can not change the receiver's volume or playback state.
//!
//! Wire format (same HomeKit framing as the control channel):
//! `[u16 LE length][ciphertext][16-byte tag]`, AAD = the length bytes, nonce =
//! per-direction block counter.
//!
//! Keys: HKDF-SHA512 over the pairing shared secret with salt `Events-Salt`.
//! The channel is mirrored compared to the control channel (OwnTone,
//! `pair_homekit.c`: "events is opposite"), so the sender encrypts with
//! `Events-Read-Encryption-Key` and decrypts with `Events-Write-Encryption-Key`.
//! If the very first frame fails authentication, the roles are swapped once and
//! the mapping that works is kept (and logged).

use std::net::SocketAddr;
use std::time::Instant;

use airplay_crypto::chacha::ControlCipher;
use airplay_crypto::hkdf::derive_key_32;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const EVENTS_SALT: &[u8] = b"Events-Salt";
const EVENTS_WRITE_INFO: &[u8] = b"Events-Write-Encryption-Key";
const EVENTS_READ_INFO: &[u8] = b"Events-Read-Encryption-Key";

/// Number of requests logged at INFO; the following ones are logged at DEBUG.
const INFO_LOGGED_REQUESTS: u64 = 10;
/// Upper bound for one buffered message, so a broken peer can not grow memory.
const MAX_MESSAGE_BYTES: usize = 1 << 20;

/// Keys for both directions of the events channel, from the sender's side.
struct EventKeys {
    /// Key for frames we send (the receiver's "read" key).
    encrypt: [u8; 32],
    /// Key for frames we receive (the receiver's "write" key).
    decrypt: [u8; 32],
}

impl EventKeys {
    fn derive(secret: &[u8]) -> Result<Self, airplay_core::error::CryptoError> {
        Ok(Self {
            encrypt: derive_key_32(secret, EVENTS_SALT, EVENTS_READ_INFO)?,
            decrypt: derive_key_32(secret, EVENTS_SALT, EVENTS_WRITE_INFO)?,
        })
    }

    fn cipher(&self) -> ControlCipher {
        ControlCipher::new(self.encrypt, self.decrypt)
    }

    fn swapped_cipher(&self) -> ControlCipher {
        ControlCipher::new(self.decrypt, self.encrypt)
    }
}

/// Start answering the receiver's requests on an established events connection.
///
/// `secret` is the pairing shared secret (the input of the control-channel key
/// derivation); it is only used to derive the keys and is not kept. The task
/// ends when the receiver closes the connection, on a protocol error, or when
/// the returned handle is aborted.
pub fn spawn<S>(stream: S, secret: &[u8], peer: SocketAddr) -> JoinHandle<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let keys = EventKeys::derive(secret);
    tokio::spawn(async move {
        match keys {
            Ok(keys) => run(stream, keys, peer).await,
            Err(err) => warn!(%peer, "events channel: key derivation failed: {err}"),
        }
    })
}

#[derive(Debug)]
enum FrameError {
    /// The receiver closed the connection (at or inside a frame).
    Closed,
    Io(std::io::Error),
    Auth,
}

async fn run<S>(mut stream: S, keys: EventKeys, peer: SocketAddr)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let started = Instant::now();
    let mut cipher = keys.cipher();
    let mut mapping_checked = false;
    let mut buffer: Vec<u8> = Vec::new();
    let mut requests: u64 = 0;

    info!(%peer, "events channel: listening for receiver requests");

    loop {
        let plaintext = match read_frame(&mut stream, &mut cipher, &keys, &mut mapping_checked).await
        {
            Ok(plaintext) => plaintext,
            Err(FrameError::Closed) => {
                info!(
                    %peer,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    requests,
                    "events channel closed by receiver"
                );
                return;
            }
            Err(FrameError::Io(err)) => {
                warn!(
                    %peer,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    requests,
                    "events channel read error: {err}"
                );
                return;
            }
            Err(FrameError::Auth) => {
                warn!(
                    %peer,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    requests,
                    "events channel decryption failed, giving up on the channel"
                );
                return;
            }
        };
        buffer.extend_from_slice(&plaintext);

        loop {
            let message = match take_message(&mut buffer) {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(reason) => {
                    warn!(%peer, requests, "events channel message parse error: {reason}");
                    return;
                }
            };
            requests += 1;
            log_message(&message, requests, peer);

            if message.is_response {
                continue; // nothing to answer
            }
            let reply = ok_response(message.cseq.as_deref());
            let encrypted = match cipher.encrypt(&reply) {
                Ok(encrypted) => encrypted,
                Err(err) => {
                    warn!(%peer, requests, "events channel encryption failed: {err}");
                    return;
                }
            };
            if let Err(err) = stream.write_all(&encrypted).await {
                info!(
                    %peer,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    requests,
                    "events channel write failed (receiver closed it?): {err}"
                );
                return;
            }
        }
    }
}

/// Read and decrypt one HomeKit frame. On the first frame, retry once with the
/// key roles swapped when authentication fails.
async fn read_frame<S>(
    stream: &mut S,
    cipher: &mut ControlCipher,
    keys: &EventKeys,
    mapping_checked: &mut bool,
) -> Result<Vec<u8>, FrameError>
where
    S: AsyncRead + Unpin,
{
    let mut length = [0u8; 2];
    read_exact(stream, &mut length).await?;
    let block_len = u16::from_le_bytes(length);
    let mut block = vec![0u8; block_len as usize + 16];
    read_exact(stream, &mut block).await?;

    match cipher.decrypt_block(&block, block_len) {
        Ok(plaintext) => {
            if !*mapping_checked {
                *mapping_checked = true;
                info!("events channel keys: expected mapping (sender encrypts with Events-Read)");
            }
            Ok(plaintext)
        }
        Err(_) if !*mapping_checked => {
            *mapping_checked = true;
            let mut swapped = keys.swapped_cipher();
            let plaintext = swapped
                .decrypt_block(&block, block_len)
                .map_err(|_| FrameError::Auth)?;
            *cipher = swapped;
            info!("events channel keys: swapped mapping (sender encrypts with Events-Write)");
            Ok(plaintext)
        }
        Err(_) => Err(FrameError::Auth),
    }
}

async fn read_exact<S>(stream: &mut S, buffer: &mut [u8]) -> Result<(), FrameError>
where
    S: AsyncRead + Unpin,
{
    match stream.read_exact(buffer).await {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Err(FrameError::Closed),
        Err(err) => Err(FrameError::Io(err)),
    }
}

/// A parsed RTSP-style message head plus the size and kind of its body.
struct Message {
    is_response: bool,
    method: String,
    uri: String,
    cseq: Option<String>,
    content_type: Option<String>,
    body_len: usize,
    body_kind: &'static str,
}

/// Remove one complete message from the front of `buffer`, if there is one.
fn take_message(buffer: &mut Vec<u8>) -> Result<Option<Message>, String> {
    let Some(head_end) = find_subslice(buffer, b"\r\n\r\n") else {
        if buffer.len() > MAX_MESSAGE_BYTES {
            return Err(format!("no header end within {} bytes", buffer.len()));
        }
        return Ok(None);
    };
    let head = std::str::from_utf8(&buffer[..head_end])
        .map_err(|_| "message head is not UTF-8".to_string())?;

    let mut lines = head.split("\r\n");
    let start_line = lines.next().unwrap_or_default();
    let mut cseq = None;
    let mut content_type = None;
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "cseq" => cseq = Some(value.to_string()),
            "content-type" => content_type = Some(value.to_string()),
            "content-length" => {
                content_length = value
                    .parse()
                    .map_err(|_| "invalid Content-Length".to_string())?;
            }
            _ => {}
        }
    }
    if content_length > MAX_MESSAGE_BYTES {
        return Err(format!("Content-Length {content_length} too large"));
    }

    let body_start = head_end + 4;
    if buffer.len() < body_start + content_length {
        return Ok(None); // body still in flight
    }

    let is_response = start_line.starts_with("RTSP/");
    let mut parts = start_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let uri = parts.next().unwrap_or_default().to_string();
    let body_kind = classify_body(&buffer[body_start..body_start + content_length]);

    let message = Message {
        is_response,
        method,
        uri,
        cseq,
        content_type,
        body_len: content_length,
        body_kind,
    };
    buffer.drain(..body_start + content_length);
    Ok(Some(message))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Coarse description of a body; the content itself is never logged.
fn classify_body(body: &[u8]) -> &'static str {
    if body.is_empty() {
        "empty"
    } else if body.starts_with(b"bplist00") {
        "bplist"
    } else if std::str::from_utf8(body).is_ok() {
        "text"
    } else {
        "binary"
    }
}

fn log_message(message: &Message, index: u64, peer: SocketAddr) {
    let kind = if message.is_response { "response" } else { "request" };
    if index <= INFO_LOGGED_REQUESTS {
        info!(
            %peer,
            index,
            kind,
            method = %message.method,
            uri = %message.uri,
            cseq = message.cseq.as_deref().unwrap_or("-"),
            content_type = message.content_type.as_deref().unwrap_or("-"),
            body_len = message.body_len,
            body_kind = message.body_kind,
            "events channel message from receiver"
        );
    } else {
        debug!(
            %peer,
            index,
            kind,
            method = %message.method,
            uri = %message.uri,
            cseq = message.cseq.as_deref().unwrap_or("-"),
            content_type = message.content_type.as_deref().unwrap_or("-"),
            body_len = message.body_len,
            body_kind = message.body_kind,
            "events channel message from receiver"
        );
    }
}

/// The only thing ever sent: `200 OK` with an empty body. `CSeq` is echoed only
/// when it is a plain number.
fn ok_response(cseq: Option<&str>) -> Vec<u8> {
    let mut reply = String::from("RTSP/1.0 200 OK\r\nServer: AirTunes/366.0\r\n");
    if let Some(cseq) = cseq.filter(|c| !c.is_empty() && c.len() <= 10 && c.bytes().all(|b| b.is_ascii_digit())) {
        reply.push_str("CSeq: ");
        reply.push_str(cseq);
        reply.push_str("\r\n");
    }
    reply.push_str("Content-Length: 0\r\n\r\n");
    reply.into_bytes()
}
