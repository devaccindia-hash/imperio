//! System audio for Imperio: WASAPI capture of the host's output, WASAPI
//! playback on the viewer, and a codec seam between them.
//!
//! The pipeline works in fixed 20 ms frames of interleaved 16-bit PCM at
//! 48 kHz stereo (the format every Opus mode and most Windows mix formats
//! agree on). Capture resamples/mixes nothing yet: it assumes the shared mix
//! is 48 kHz stereo (the Windows default) and converts float samples to i16.
//!
//! Compression lives behind [`AudioEncoder`] / [`AudioDecoder`], mirroring the
//! video [`imperio_codec::VideoEncoder`] seam. [`PcmCodec`] is the always-
//! available default (raw little-endian i16, ~1.5 Mbit/s). Opus slots in here
//! once a pure-Rust or statically-linkable libopus backend is wired; the wire
//! protocol already carries a codec tag so the two ends negotiate it.

pub mod capture;
pub mod playback;

/// Samples per second, per channel.
pub const SAMPLE_RATE: u32 = 48_000;
/// Interleaved channel count.
pub const CHANNELS: u16 = 2;
/// Frame length in milliseconds. 20 ms is Opus's sweet spot and small enough
/// for interactive latency.
pub const FRAME_MS: u32 = 20;
/// Samples per channel in one frame (48_000 / 1000 * 20 = 960).
pub const SAMPLES_PER_FRAME: usize = (SAMPLE_RATE as usize / 1000) * FRAME_MS as usize;
/// Interleaved i16 count in one frame (960 * 2 = 1920).
pub const INTERLEAVED_PER_FRAME: usize = SAMPLES_PER_FRAME * CHANNELS as usize;

/// Largest audio payload to put in one datagram, kept under the link's MTU
/// budget and aligned to 4 bytes (one stereo i16 sample pair) so a chunked
/// raw-PCM payload never splits mid-sample. Compressed frames (Opus) are far
/// smaller than this and always travel in a single datagram.
pub const MAX_AUDIO_PAYLOAD: usize = 1100;

/// Codec tag carried on the wire so the decoder matches the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioCodec {
    Pcm = 0,
    Opus = 1,
}

impl AudioCodec {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(AudioCodec::Pcm),
            1 => Some(AudioCodec::Opus),
            _ => None,
        }
    }
}

/// Encodes one 20 ms frame of interleaved i16 PCM into a wire payload.
pub trait AudioEncoder: Send {
    /// `pcm` is [`INTERLEAVED_PER_FRAME`] interleaved samples. Returns the
    /// encoded bytes for this frame.
    fn encode(&mut self, pcm: &[i16]) -> anyhow::Result<Vec<u8>>;
    fn codec(&self) -> AudioCodec;
}

/// Decodes a wire payload back into interleaved i16 PCM.
pub trait AudioDecoder: Send {
    /// Writes interleaved samples into `out` and returns the number of samples
    /// per channel decoded. `out` must hold at least [`INTERLEAVED_PER_FRAME`].
    fn decode(&mut self, data: &[u8], out: &mut [i16]) -> anyhow::Result<usize>;
    fn codec(&self) -> AudioCodec;
}

/// Raw little-endian 16-bit PCM passthrough. No compression, no dependencies,
/// always builds and runs; the baseline until Opus is wired.
#[derive(Default)]
pub struct PcmCodec;

impl AudioEncoder for PcmCodec {
    fn encode(&mut self, pcm: &[i16]) -> anyhow::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(pcm.len() * 2);
        for &s in pcm {
            out.extend_from_slice(&s.to_le_bytes());
        }
        Ok(out)
    }
    fn codec(&self) -> AudioCodec {
        AudioCodec::Pcm
    }
}

impl AudioDecoder for PcmCodec {
    fn decode(&mut self, data: &[u8], out: &mut [i16]) -> anyhow::Result<usize> {
        let n = (data.len() / 2).min(out.len());
        for (i, o) in out.iter_mut().enumerate().take(n) {
            *o = i16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
        }
        Ok(n / CHANNELS as usize)
    }
    fn codec(&self) -> AudioCodec {
        AudioCodec::Pcm
    }
}

/// Convert a slice of 32-bit float samples (-1.0..=1.0) to clamped i16.
pub fn f32_to_i16(src: &[f32], dst: &mut Vec<i16>) {
    dst.clear();
    dst.reserve(src.len());
    for &s in src {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round();
        dst.push(v as i16);
    }
}

/// Convert interleaved i16 to 32-bit float (-1.0..=1.0).
pub fn i16_to_f32(src: &[i16], dst: &mut Vec<f32>) {
    dst.clear();
    dst.reserve(src.len());
    for &s in src {
        dst.push(s as f32 / i16::MAX as f32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_dimensions() {
        assert_eq!(SAMPLES_PER_FRAME, 960);
        assert_eq!(INTERLEAVED_PER_FRAME, 1920);
    }

    #[test]
    fn pcm_round_trips() {
        let frame: Vec<i16> = (0..INTERLEAVED_PER_FRAME as i32)
            .map(|i| (i - 900) as i16)
            .collect();
        let mut enc = PcmCodec;
        let bytes = enc.encode(&frame).unwrap();
        assert_eq!(bytes.len(), INTERLEAVED_PER_FRAME * 2);
        let mut dec = PcmCodec;
        let mut out = vec![0i16; INTERLEAVED_PER_FRAME];
        let per_channel = dec.decode(&bytes, &mut out).unwrap();
        assert_eq!(per_channel, SAMPLES_PER_FRAME);
        assert_eq!(out, frame);
    }

    #[test]
    fn float_conversion_clamps() {
        let src = [0.0f32, 1.0, -1.0, 2.0, -2.0];
        let mut i16s = Vec::new();
        f32_to_i16(&src, &mut i16s);
        assert_eq!(i16s[0], 0);
        assert_eq!(i16s[1], i16::MAX);
        assert_eq!(i16s[2], -i16::MAX);
        assert_eq!(i16s[3], i16::MAX); // clamped
        assert_eq!(i16s[4], -i16::MAX); // clamped
    }

    #[test]
    fn codec_tag_round_trips() {
        assert_eq!(AudioCodec::from_u8(AudioCodec::Pcm as u8), Some(AudioCodec::Pcm));
        assert_eq!(AudioCodec::from_u8(AudioCodec::Opus as u8), Some(AudioCodec::Opus));
        assert_eq!(AudioCodec::from_u8(7), None);
    }
}
