/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A minimal [RFC 6455](https://datatracker.ietf.org/doc/html/rfc6455) WebSocket
//! implementation used by the Chrome DevTools Protocol (CDP) server.
//!
//! Only the server side of the protocol is implemented, which means that:
//! - frames sent to the client are never masked,
//! - frames received from the client are required to be masked.
//!
//! The implementation deliberately avoids depending on a full WebSocket
//! library; the CDP server only needs text frames plus the ping/pong and
//! close control frames.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use sha1::{Digest, Sha1};

/// The fixed GUID appended to the client key when computing the
/// `Sec-WebSocket-Accept` response header value.
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Computes the value of the `Sec-WebSocket-Accept` header for a given client
/// `Sec-WebSocket-Key`, as specified by
/// [RFC 6455, section 4.2.2](https://datatracker.ietf.org/doc/html/rfc6455#section-4.2.2).
pub(crate) fn websocket_accept_key(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(WEBSOCKET_GUID.as_bytes());
    BASE64.encode(hasher.finalize())
}

/// A WebSocket message, limited to the opcodes the CDP server needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WsMessage {
    /// A UTF-8 text message.
    Text(String),
    /// A binary message.
    Binary(Vec<u8>),
    /// A pong control frame, sent in response to a ping.
    Pong(Vec<u8>),
    /// A close control frame with an optional close reason.
    Close(Option<String>),
}

impl WsMessage {
    /// The RFC 6455 opcode for this message.
    fn opcode(&self) -> u8 {
        match self {
            WsMessage::Text(_) => 0x1,
            WsMessage::Binary(_) => 0x2,
            WsMessage::Close(_) => 0x8,
            WsMessage::Pong(_) => 0xA,
        }
    }

    /// The payload bytes for this message.
    fn payload(&self) -> Vec<u8> {
        match self {
            WsMessage::Text(text) => text.as_bytes().to_vec(),
            WsMessage::Binary(bytes) | WsMessage::Pong(bytes) => bytes.clone(),
            WsMessage::Close(reason) => {
                // A close frame carries a 2-byte status code followed by an
                // optional UTF-8 reason.
                let mut payload = 1000u16.to_be_bytes().to_vec();
                if let Some(reason) = reason {
                    payload.extend_from_slice(reason.as_bytes());
                }
                payload
            },
        }
    }
}

/// A single, parsed WebSocket frame.
struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

/// Reads a single frame from `reader`. Returns `Ok(None)` on a clean EOF
/// before any bytes of a new frame were read.
fn read_frame<R: Read>(reader: &mut R) -> io::Result<Option<Frame>> {
    // Read the two header bytes one at a time: `read` on a stream is allowed
    // to return short reads, so `read_exact` must be used for everything
    // after detecting a clean EOF on the very first byte.
    let mut first = [0u8; 1];
    if reader.read(&mut first)? == 0 {
        return Ok(None);
    }
    let mut second = [0u8; 1];
    reader.read_exact(&mut second)?;
    let header = [first[0], second[0]];

    let fin = header[0] & 0x80 != 0;
    let opcode = header[0] & 0x0F;
    let masked = header[1] & 0x80 != 0;
    let initial_length = u64::from(header[1] & 0x7F);

    // Clients must always mask their frames. Reject unmasked ones early.
    if !masked {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "received unmasked WebSocket frame from client",
        ));
    }

    // Control frames must have a payload length of 125 bytes or fewer.
    if opcode >= 0x8 && initial_length > 125 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WebSocket control frame with payload larger than 125 bytes",
        ));
    }

    let payload_length = match initial_length {
        126 => {
            let mut buffer = [0u8; 2];
            reader.read_exact(&mut buffer)?;
            u16::from_be_bytes(buffer) as u64
        },
        127 => {
            let mut buffer = [0u8; 8];
            reader.read_exact(&mut buffer)?;
            u64::from_be_bytes(buffer)
        },
        length => length,
    };

    // Guard against absurd frame sizes (the CDP protocol only exchanges
    // relatively small JSON messages).
    if payload_length > 64 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WebSocket frame payload too large",
        ));
    }

    let mut mask = [0u8; 4];
    reader.read_exact(&mut mask)?;

    let mut payload = vec![0u8; payload_length as usize];
    reader.read_exact(&mut payload)?;
    apply_mask(&mut payload, &mask);

    Ok(Some(Frame { fin, opcode, payload }))
}

/// XORs the payload bytes with the four-byte masking key.
fn apply_mask(payload: &mut [u8], mask: &[u8; 4]) {
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
}

/// Writes a single, unmasked frame (frames sent by a server are never masked).
fn write_frame<W: Write>(writer: &mut W, fin: bool, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let mut header = [0u8; 2];
    header[0] = if fin { 0x80 } else { 0x00 } | (opcode & 0x0F);

    let length = payload.len();
    if length < 126 {
        header[1] = length as u8;
        writer.write_all(&header)?;
    } else if length <= u16::MAX as usize {
        header[1] = 126;
        writer.write_all(&header)?;
        writer.write_all(&(length as u16).to_be_bytes())?;
    } else {
        header[1] = 127;
        writer.write_all(&header)?;
        writer.write_all(&(length as u64).to_be_bytes())?;
    }
    writer.write_all(payload)?;
    writer.flush()
}

/// Parses an HTTP/1.1 request (headers only, no body) from `reader`. This is
/// used to read the WebSocket opening handshake. Returns the request path and
/// the request headers with lower-cased names.
pub(crate) fn read_http_request<R: Read>(
    reader: &mut R,
) -> io::Result<(String, Vec<(String, String)>)> {
    let mut buffer = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        if reader.read(&mut byte)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed during WebSocket handshake",
            ));
        }
        buffer.push(byte[0]);
        // The end of the headers is signaled by an empty line.
        if buffer.ends_with(b"\r\n\r\n") || buffer.ends_with(b"\n\n") {
            break;
        }
        if buffer.len() > 16 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WebSocket handshake request too large",
            ));
        }
    }

    let request = String::from_utf8_lossy(&buffer).into_owned();
    let mut lines = request.lines();
    let request_line = lines.next().unwrap_or_default().to_owned();
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    Ok((path, headers))
}

/// A WebSocket connection that has completed its opening handshake. The
/// underlying stream is shared between a reader and a writer half, which may
/// be used from different threads.
#[derive(Clone)]
pub(crate) struct WsStream {
    /// Handle to use for receiving messages from the client.
    receiver: Arc<Mutex<TcpStream>>,
    /// Handle to use for sending messages to the client.
    sender: Arc<Mutex<TcpStream>>,
}

impl WsStream {
    /// Completes the WebSocket opening handshake, given the client's
    /// `Sec-WebSocket-Key` that the caller has already read from the stream.
    pub(crate) fn accept_with_key(mut stream: TcpStream, key: &str) -> io::Result<Self> {
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {}\r\n\r\n",
            websocket_accept_key(key)
        );
        stream.write_all(response.as_bytes())?;
        stream.flush()?;

        let receiver = Arc::new(Mutex::new(stream.try_clone()?));
        let sender = Arc::new(Mutex::new(stream));
        Ok(Self { receiver, sender })
    }

    /// Removes the handshake-phase read timeout so the reader half can
    /// block indefinitely waiting for client messages.
    pub(crate) fn clear_read_timeout(&self) {
        let _ = self.receiver.lock().unwrap().set_read_timeout(None);
        let _ = self.sender.lock().unwrap().set_read_timeout(None);
    }

    /// Splits the stream into an independent receiver and writer half.
    pub(crate) fn split(self) -> (WsReceiver, WsWriter) {
        (
            WsReceiver {
                receiver: self.receiver,
            },
            WsWriter { sender: self.sender },
        )
    }
}

/// The receiving half of a WebSocket connection.
pub(crate) struct WsReceiver {
    receiver: Arc<Mutex<TcpStream>>,
}

impl WsReceiver {
    /// Reads the next message, transparently handling fragmentation and
    /// answering pings with pongs. Returns `Ok(None)` once the client closed
    /// the connection.
    pub(crate) fn read_message(&mut self) -> io::Result<Option<WsMessage>> {
        let mut stream = self.receiver.lock().unwrap();
        let mut fragments: Vec<u8> = Vec::new();
        let mut fragment_opcode: Option<u8> = None;

        loop {
            let Some(frame) = read_frame(&mut *stream)? else {
                return Ok(None);
            };

            match frame.opcode {
                0x0 if !frame.fin => {
                    // A continuation frame of a fragmented message.
                    if fragment_opcode.is_none() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "unexpected continuation frame",
                        ));
                    }
                    fragments.extend_from_slice(&frame.payload);
                },
                0x0 => {
                    // A final continuation frame, completing the message.
                    let Some(opcode) = fragment_opcode.take() else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "unexpected continuation frame",
                        ));
                    };
                    fragments.extend_from_slice(&frame.payload);
                    return Ok(Some(decode_data_message(opcode, fragments)?));
                },
                0x1 | 0x2 => {
                    if frame.fin {
                        return Ok(Some(decode_data_message(frame.opcode, frame.payload)?));
                    }
                    // The first frame of a fragmented message.
                    if fragment_opcode.is_some() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "nested fragmented messages are not allowed",
                        ));
                    }
                    fragment_opcode = Some(frame.opcode);
                    fragments = frame.payload;
                },
                0x8 => {
                    // Echo the close frame back and report the closed
                    // connection to the caller.
                    let reason = String::from_utf8(frame.payload.clone().into_iter().skip(2).collect())
                        .ok()
                        .filter(|reason| !reason.is_empty());
                    let _ = write_frame(&mut *stream, true, 0x8, &frame.payload);
                    return Ok(Some(WsMessage::Close(reason)));
                },
                0x9 => {
                    // Ping: answer with an equivalent pong and keep reading.
                    let pong = WsMessage::Pong(frame.payload.clone());
                    let _ = write_frame(&mut *stream, true, pong.opcode(), &pong.payload());
                },
                0xA => {
                    // Unsolicited pongs are ignored.
                },
                opcode => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unsupported WebSocket opcode: {opcode}"),
                    ));
                },
            }
        }
    }
}

/// Decodes a completed data frame or message into a [`WsMessage`].
fn decode_data_message(opcode: u8, payload: Vec<u8>) -> io::Result<WsMessage> {
    match opcode {
        0x1 => String::from_utf8(payload)
            .map(WsMessage::Text)
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "WebSocket text frame is not UTF-8")
            }),
        0x2 => Ok(WsMessage::Binary(payload)),
        _ => unreachable!("only data opcodes are passed to decode_data_message"),
    }
}

/// The sending half of a WebSocket connection.
#[derive(Clone)]
pub(crate) struct WsWriter {
    sender: Arc<Mutex<TcpStream>>,
}

impl WsWriter {
    /// Sends a message to the client, fragmenting payloads larger than a
    /// single frame into 1 MiB chunks.
    pub(crate) fn write_message(&mut self, message: &WsMessage) -> io::Result<()> {
        let mut stream = self.sender.lock().unwrap();
        let opcode = message.opcode();
        let payload = message.payload();

        const MAX_FRAGMENT_SIZE: usize = 1024 * 1024;
        if payload.len() <= MAX_FRAGMENT_SIZE {
            return write_frame(&mut *stream, true, opcode, &payload);
        }

        for (index, chunk) in payload.chunks(MAX_FRAGMENT_SIZE).enumerate() {
            let fin = index == payload.len().div_ceil(MAX_FRAGMENT_SIZE) - 1;
            let frame_opcode = if index == 0 { opcode } else { 0x0 };
            write_frame(&mut *stream, fin, frame_opcode, chunk)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(header_name, _)| header_name == name)
            .map(|(_, value)| value.as_str())
    }

    /// Encodes a client frame (masked, as required by RFC 6455) into a buffer.
    fn encode_client_frame(fin: bool, opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.push(if fin { 0x80 } else { 0x00 } | (opcode & 0x0F));
        let length = payload.len();
        if length < 126 {
            frame.push(0x80 | length as u8);
        } else if length <= u16::MAX as usize {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(length as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(length as u64).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        let mut masked_payload = payload.to_vec();
        apply_mask(&mut masked_payload, &mask);
        frame.extend_from_slice(&masked_payload);
        frame
    }

    #[test]
    fn test_websocket_accept_key() {
        // The example from RFC 6455, section 1.3.
        assert_eq!(
            websocket_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn test_frame_roundtrip() {
        let mask = [0x01, 0x02, 0x03, 0x04];
        let payload = b"Hello, WebSocket!".to_vec();
        let encoded = encode_client_frame(true, 0x1, &payload, mask);

        let mut reader = io::Cursor::new(encoded);
        let frame = read_frame(&mut reader).unwrap().expect("a frame");
        assert!(frame.fin);
        assert_eq!(frame.opcode, 0x1);
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn test_fragmented_message() {
        let mask = [0xA0, 0xA1, 0xA2, 0xA3];
        let mut encoded = encode_client_frame(false, 0x1, b"first ", mask);
        encoded.extend_from_slice(&encode_client_frame(false, 0x0, b"second ", mask));
        encoded.extend_from_slice(&encode_client_frame(true, 0x0, b"third", mask));

        let mut receiver_bytes = io::Cursor::new(encoded);
        // Fragmentation handling lives in `WsReceiver`; replicate its loop
        // here over raw frames.
        let mut fragments = Vec::new();
        let mut opcode = None;
        let message = loop {
            let frame = read_frame(&mut receiver_bytes).unwrap().expect("a frame");
            if frame.opcode == 0x0 {
                fragments.extend_from_slice(&frame.payload);
                if frame.fin {
                    break decode_data_message(opcode.unwrap(), fragments).unwrap();
                }
            } else if frame.fin {
                break decode_data_message(frame.opcode, frame.payload).unwrap();
            } else {
                opcode = Some(frame.opcode);
                fragments = frame.payload;
            }
        };
        assert_eq!(message, WsMessage::Text("first second third".to_owned()));
    }

    #[test]
    fn test_large_payload_length() {
        let mask = [0xFF, 0xFF, 0xFF, 0xFF];
        let payload = vec![0x42u8; 70_000];
        let encoded = encode_client_frame(true, 0x2, &payload, mask);

        let mut reader = io::Cursor::new(encoded);
        let frame = read_frame(&mut reader).unwrap().expect("a frame");
        assert_eq!(frame.opcode, 0x2);
        assert_eq!(frame.payload.len(), 70_000);
        assert!(frame.payload.iter().all(|byte| *byte == 0x42));
    }

    #[test]
    fn test_write_frame_roundtrip_through_read() {
        // A frame written by the server (unmasked) must be decodable by a
        // masked-aware reader only if we simulate the masking, so instead
        // verify the exact bytes for a small unmasked text frame.
        let mut buffer = Vec::new();
        write_frame(&mut buffer, true, 0x1, b"hi").unwrap();
        assert_eq!(buffer, vec![0x81, 0x02, b'h', b'i']);
    }

    #[test]
    fn test_http_request_parsing() {
        let request = b"GET /devtools/browser/abc HTTP/1.1\r\n\
                        Host: localhost:9222\r\n\
                        Upgrade: WebSocket\r\n\
                        Connection: keep-alive, Upgrade\r\n\
                        Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                        Sec-WebSocket-Version: 13\r\n\r\n";
        let mut reader = io::Cursor::new(&request[..]);
        let (path, headers) = read_http_request(&mut reader).unwrap();
        assert_eq!(path, "/devtools/browser/abc");
        assert_eq!(
            header_value(&headers, "sec-websocket-key"),
            Some("dGhlIHNhbXBsZSBub25jZQ==")
        );
        assert!(header_value(&headers, "upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket")));
        assert!(header_value(&headers, "connection")
            .is_some_and(|value| value.to_ascii_lowercase().contains("upgrade")));
    }

    #[test]
    fn test_close_payload() {
        let message = WsMessage::Close(Some("done".to_owned()));
        let payload = message.payload();
        assert_eq!(&payload[..2], &1000u16.to_be_bytes());
        assert_eq!(&payload[2..], b"done");
        assert_eq!(message.opcode(), 0x8);
    }
}
