//! TCP channel transport (P3): length-prefixed frame I/O over a real socket.
//!
//! java-tron's channel wraps each [`Frame`] in a 4-byte big-endian length prefix
//! on the TCP stream. This module provides async read/write of framed messages
//! and a minimal handshake (exchange `P2pHello`), proving the wire protocol works
//! over real sockets. The full peer state machine (sync, keepalive) builds on this.

use crate::message::{Frame, FrameError, MessageType, MAX_MESSAGE_SIZE};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug)]
pub enum ChannelError {
    Io(std::io::Error),
    Frame(FrameError),
    TooLarge(usize),
}

impl From<std::io::Error> for ChannelError {
    fn from(e: std::io::Error) -> Self {
        ChannelError::Io(e)
    }
}

/// Write a frame with a 4-byte big-endian length prefix.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    frame: &Frame,
) -> Result<(), ChannelError> {
    let body = frame.encode();
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one length-prefixed frame, rejecting oversized declarations before reading.
pub async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Frame, ChannelError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_SIZE + 1 {
        return Err(ChannelError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Frame::decode(&body).map_err(ChannelError::Frame)
}

/// Respond to an inbound handshake: read the peer's `P2pHello`, reply with ours.
/// Returns the peer's hello payload.
pub async fn accept_handshake<S>(stream: &mut S, our_hello: Vec<u8>) -> Result<Vec<u8>, ChannelError>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let peer = read_frame(stream).await?;
    if peer.kind != MessageType::P2pHello {
        return Err(ChannelError::Frame(FrameError::UnknownType(peer.kind as u8)));
    }
    write_frame(stream, &Frame::new(MessageType::P2pHello, our_hello)).await?;
    Ok(peer.payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handshake_over_real_tcp_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Server: accept one connection and complete the handshake.
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            accept_handshake(&mut sock, b"server-hello".to_vec()).await.unwrap()
        });

        // Client: connect, send hello, read the server's hello back.
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        write_frame(&mut client, &Frame::new(MessageType::P2pHello, b"client-hello".to_vec()))
            .await
            .unwrap();
        let reply = read_frame(&mut client).await.unwrap();

        assert_eq!(reply.kind, MessageType::P2pHello);
        assert_eq!(reply.payload, b"server-hello");
        // Server observed the client's hello payload.
        assert_eq!(server.await.unwrap(), b"client-hello");
    }

    #[tokio::test]
    async fn oversized_length_prefix_rejected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Announce a huge length, then close.
            let _ = sock.write_all(&(u32::MAX).to_be_bytes()).await;
        });
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        assert!(matches!(read_frame(&mut client).await, Err(ChannelError::TooLarge(_))));
    }
}
