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

/// Encode a list of block numbers as concatenated 8-byte big-endian integers.
fn encode_numbers(nums: &[i64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nums.len() * 8);
    for n in nums {
        out.extend_from_slice(&n.to_be_bytes());
    }
    out
}

fn decode_numbers(bytes: &[u8]) -> Vec<i64> {
    bytes
        .chunks_exact(8)
        .map(|c| i64::from_be_bytes(c.try_into().unwrap()))
        .collect()
}

/// **Server side** of block sync: read the peer's `SyncBlockChain` (its head),
/// reply with a `BlockInventory` of the block numbers we can offer above it, then
/// serve each `FetchInvData` request with the corresponding `Block`. `have` is our
/// sorted block numbers; `block_bytes(n)` returns the encoded block for number `n`.
/// Returns when the peer stops requesting (EOF).
pub async fn serve_sync<S, F>(
    stream: &mut S,
    have: &[i64],
    block_bytes: F,
) -> Result<usize, ChannelError>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
    F: Fn(i64) -> Option<Vec<u8>>,
{
    let req = read_frame(stream).await?;
    if req.kind != MessageType::SyncBlockChain {
        return Err(ChannelError::Frame(FrameError::UnknownType(req.kind as u8)));
    }
    let peer_head = decode_numbers(&req.payload).first().copied().unwrap_or(-1);
    let offer: Vec<i64> = have.iter().copied().filter(|&n| n > peer_head).collect();
    write_frame(stream, &Frame::new(MessageType::BlockInventory, encode_numbers(&offer))).await?;

    let mut served = 0usize;
    loop {
        match read_frame(stream).await {
            Ok(f) if f.kind == MessageType::FetchInvData => {
                let n = decode_numbers(&f.payload).first().copied().unwrap_or(-1);
                let payload = block_bytes(n).unwrap_or_default();
                write_frame(stream, &Frame::new(MessageType::Block, payload)).await?;
                served += 1;
            }
            Ok(_) => break,
            Err(ChannelError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
    }
    Ok(served)
}

/// **Client side** of block sync: announce our head, read the offered inventory,
/// fetch each offered block, and return the received `(number, block_bytes)` pairs.
pub async fn sync_from<S>(stream: &mut S, our_head: i64) -> Result<Vec<(i64, Vec<u8>)>, ChannelError>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    write_frame(stream, &Frame::new(MessageType::SyncBlockChain, encode_numbers(&[our_head]))).await?;
    let inv = read_frame(stream).await?;
    if inv.kind != MessageType::BlockInventory {
        return Err(ChannelError::Frame(FrameError::UnknownType(inv.kind as u8)));
    }
    let numbers = decode_numbers(&inv.payload);
    let mut out = Vec::with_capacity(numbers.len());
    for n in numbers {
        write_frame(stream, &Frame::new(MessageType::FetchInvData, encode_numbers(&[n]))).await?;
        let block = read_frame(stream).await?;
        out.push((n, block.payload));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn block_sync_over_real_tcp() {
        // Server has "blocks" 1..=5 (payload = the number as a marker); client at
        // head 2 should receive blocks 3,4,5 over the wire.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            serve_sync(&mut sock, &[1, 2, 3, 4, 5], |n| Some(format!("block-{n}").into_bytes()))
                .await
                .unwrap()
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let fetched = sync_from(&mut client, 2).await.unwrap();
        drop(client); // EOF -> server loop ends

        assert_eq!(fetched.len(), 3);
        assert_eq!(fetched[0], (3, b"block-3".to_vec()));
        assert_eq!(fetched[2], (5, b"block-5".to_vec()));
        assert_eq!(server.await.unwrap(), 3); // served three blocks
    }

    #[tokio::test]
    async fn sync_with_nothing_to_offer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let _ = serve_sync(&mut sock, &[1, 2], |_| None).await;
        });
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        // client already ahead -> empty inventory
        let fetched = sync_from(&mut client, 5).await.unwrap();
        assert!(fetched.is_empty());
    }

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
