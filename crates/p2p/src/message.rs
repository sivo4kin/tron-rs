//! Tron p2p wire messages: framing and types.
//!
//! On the TCP channel a message is a single **type byte** followed by its
//! protobuf-encoded payload (java-tron `MessageTypes` + `Message.getData`).
//! Type codes match java-tron `org.tron.core.net.message.MessageTypes`.

/// Tron p2p message type codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    Trx = 0x01,
    Block = 0x02,
    Trxs = 0x03,
    Blocks = 0x04,
    BlockHeaders = 0x05,
    Inventory = 0x06,
    FetchInvData = 0x07,
    SyncBlockChain = 0x08,
    BlockChainInventory = 0x09,
    FetchBlockHeaders = 0x11,
    BlockInventory = 0x12,
    TrxInventory = 0x13,
    P2pHello = 0x20,
    P2pDisconnect = 0x21,
    P2pPing = 0x22,
    P2pPong = 0x23,
}

impl MessageType {
    pub fn from_u8(b: u8) -> Option<Self> {
        use MessageType::*;
        Some(match b {
            0x01 => Trx,
            0x02 => Block,
            0x03 => Trxs,
            0x04 => Blocks,
            0x05 => BlockHeaders,
            0x06 => Inventory,
            0x07 => FetchInvData,
            0x08 => SyncBlockChain,
            0x09 => BlockChainInventory,
            0x11 => FetchBlockHeaders,
            0x12 => BlockInventory,
            0x13 => TrxInventory,
            0x20 => P2pHello,
            0x21 => P2pDisconnect,
            0x22 => P2pPing,
            0x23 => P2pPong,
            _ => return None,
        })
    }
}

/// A framed p2p message: a type byte plus a protobuf payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: MessageType,
    pub payload: Vec<u8>,
}

/// Maximum accepted p2p message payload (java-tron caps message body size to
/// resist memory-exhaustion DoS; ~5 MB is the effective block/message ceiling).
pub const MAX_MESSAGE_SIZE: usize = 5 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    Empty,
    UnknownType(u8),
    /// Payload exceeds [`MAX_MESSAGE_SIZE`] — rejected before allocation/parsing.
    TooLarge(usize),
}

impl Frame {
    pub fn new(kind: MessageType, payload: Vec<u8>) -> Self {
        Self { kind, payload }
    }

    /// Encode as `[type_byte][payload]`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + self.payload.len());
        out.push(self.kind as u8);
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decode a `[type_byte][payload]` frame, rejecting oversized payloads before
    /// allocating (DoS hardening).
    pub fn decode(bytes: &[u8]) -> Result<Frame, FrameError> {
        let (&first, rest) = bytes.split_first().ok_or(FrameError::Empty)?;
        if rest.len() > MAX_MESSAGE_SIZE {
            return Err(FrameError::TooLarge(rest.len()));
        }
        let kind = MessageType::from_u8(first).ok_or(FrameError::UnknownType(first))?;
        Ok(Frame::new(kind, rest.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_codes_match_java_tron() {
        assert_eq!(MessageType::P2pHello as u8, 0x20);
        assert_eq!(MessageType::SyncBlockChain as u8, 0x08);
        assert_eq!(MessageType::BlockInventory as u8, 0x12);
        assert_eq!(MessageType::from_u8(0x02), Some(MessageType::Block));
        assert_eq!(MessageType::from_u8(0x99), None);
    }

    #[test]
    fn frame_roundtrip() {
        let f = Frame::new(MessageType::Block, vec![1, 2, 3, 4]);
        let bytes = f.encode();
        assert_eq!(bytes[0], 0x02);
        assert_eq!(&bytes[1..], &[1, 2, 3, 4]);
        assert_eq!(Frame::decode(&bytes).unwrap(), f);
    }

    #[test]
    fn empty_and_unknown_frames_error() {
        assert_eq!(Frame::decode(&[]), Err(FrameError::Empty));
        assert_eq!(Frame::decode(&[0x99, 0x00]), Err(FrameError::UnknownType(0x99)));
    }

    #[test]
    fn empty_payload_is_valid() {
        let f = Frame::new(MessageType::P2pPing, vec![]);
        assert_eq!(Frame::decode(&f.encode()).unwrap(), f);
    }

    #[test]
    fn oversized_payload_rejected_before_alloc() {
        // A type byte followed by a payload one over the cap must be rejected.
        let mut buf = vec![MessageType::Block as u8];
        buf.resize(1 + MAX_MESSAGE_SIZE + 1, 0);
        assert!(matches!(Frame::decode(&buf), Err(FrameError::TooLarge(_))));
        // At exactly the cap it is accepted.
        let mut ok = vec![MessageType::Block as u8];
        ok.resize(1 + MAX_MESSAGE_SIZE, 0);
        assert!(Frame::decode(&ok).is_ok());
    }
}
