//! Video encode / decode and the D3D11 plumbing around it.
//!
//! Phase 1 backend: Media Foundation H.264 transforms (hardware MFT when the
//! GPU exposes one, Microsoft's software encoder otherwise) driven through
//! D3D11 so frames never leave the GPU. The `VideoEncoder` / `VideoDecoder`
//! traits are the seam where NVENC / AMF / QuickSync SDK backends slot in
//! later; the research found inbox MF hardware MFTs unreliable on some
//! current drivers, which is why the fallback exists from day one.

pub mod d3d;
pub mod mf;
pub mod record;

use std::time::Duration;

use imperio_protocol::VideoCodec;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

/// One encoded access unit plus timing used by the bench tool.
pub struct EncodedFrame {
    pub codec: VideoCodec,
    pub data: bytes::Bytes,
    pub keyframe: bool,
    /// Presentation time in 100 ns units, as given to the encoder.
    pub pts_100ns: i64,
    /// Wall-clock time the encoder spent on this frame.
    pub encode_time: Duration,
}

/// Encoder tuning for interactive desktop streaming.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Long GOP; keyframes are forced explicitly on packet loss instead.
    pub gop_frames: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            codec: VideoCodec::H264,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 8_000,
            gop_frames: 600,
        }
    }
}

/// Common interface over hardware and software encoders.
pub trait VideoEncoder {
    /// Encode one NV12 texture living on the encoder's device. Hardware
    /// encoders pipeline, so zero or more frames may come out per call.
    fn encode(
        &mut self,
        nv12: &ID3D11Texture2D,
        pts_100ns: i64,
        force_keyframe: bool,
    ) -> anyhow::Result<Vec<EncodedFrame>>;
    /// Change the target bitrate on the fly (congestion control).
    fn set_bitrate(&mut self, kbps: u32) -> anyhow::Result<()>;
    fn config(&self) -> &EncoderConfig;
    fn name(&self) -> &str;
}

/// A decoded NV12 frame. Holds the decoder's output sample so the texture
/// stays valid; drop it after rendering to return the surface to the pool.
pub struct DecodedFrame {
    pub texture: ID3D11Texture2D,
    pub subresource: u32,
    pub width: u32,
    pub height: u32,
    pub pts_100ns: i64,
    _sample: windows::Win32::Media::MediaFoundation::IMFSample,
}

pub trait VideoDecoder {
    fn decode(&mut self, data: &[u8], pts_100ns: i64) -> anyhow::Result<Vec<DecodedFrame>>;
    fn name(&self) -> &str;
}
