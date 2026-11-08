//! Frame <-> chunk framing carried as media payloads inside the encrypted
//! link. Loss recovery happens one layer down (`link`), ordering and
//! deadlines one layer up (`jitter`).

use bytes::{Buf, BufMut, Bytes, BytesMut};

/// Payload bytes per chunk. The link adds 1 + 4 + 8 bytes of inner header,
/// 1 + 8 + 16 bytes of outer header and AEAD tag; the total stays under a
/// 1280-byte IPv6 minimum MTU.
pub const MAX_PAYLOAD: usize = 1150;

/// frame_id u32 | chunk_index u16 | chunk_count u16 | flags u8 | timestamp_us u64
const HEADER_LEN: usize = 4 + 2 + 2 + 1 + 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHeader {
    pub frame_id: u32,
    pub index: u16,
    pub count: u16,
    pub keyframe: bool,
    /// Host wall clock (microseconds since UNIX epoch) when the frame was
    /// captured. Only meaningful for latency measurement when host and viewer
    /// share a clock (same machine).
    pub timestamp_us: u64,
}

pub fn split(frame_id: u32, keyframe: bool, timestamp_us: u64, data: &[u8]) -> Vec<Bytes> {
    let count = data.len().div_ceil(MAX_PAYLOAD).max(1);
    assert!(
        count <= u16::MAX as usize,
        "frame too large for chunk index"
    );
    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[][..]]
    } else {
        data.chunks(MAX_PAYLOAD).collect()
    };
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut buf = BytesMut::with_capacity(HEADER_LEN + chunk.len());
            buf.put_u32(frame_id);
            buf.put_u16(i as u16);
            buf.put_u16(count as u16);
            buf.put_u8(keyframe as u8);
            buf.put_u64(timestamp_us);
            buf.put_slice(chunk);
            buf.freeze()
        })
        .collect()
}

pub fn parse(mut body: Bytes) -> Option<(ChunkHeader, Bytes)> {
    if body.len() < HEADER_LEN {
        return None;
    }
    let header = ChunkHeader {
        frame_id: body.get_u32(),
        index: body.get_u16(),
        count: body.get_u16(),
        keyframe: body.get_u8() != 0,
        timestamp_us: body.get_u64(),
    };
    Some((header, body))
}

/// A fully reassembled frame.
#[derive(Debug, Clone)]
pub struct CompleteFrame {
    pub frame_id: u32,
    pub keyframe: bool,
    pub timestamp_us: u64,
    pub data: Bytes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_header() {
        let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let chunks = split(7, true, 42, &data);
        assert_eq!(chunks.len(), 5);
        let (h, p) = parse(chunks[4].clone()).unwrap();
        assert_eq!(
            (h.frame_id, h.index, h.count, h.keyframe, h.timestamp_us),
            (7, 4, 5, true, 42)
        );
        assert_eq!(&p[..], &data[4 * MAX_PAYLOAD..]);
    }

    #[test]
    fn empty_frame_is_one_chunk() {
        assert_eq!(split(1, false, 0, &[]).len(), 1);
    }
}
