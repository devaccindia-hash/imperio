//! Screen capture.
//!
//! Primary path: DXGI Desktop Duplication (GPU texture out, dirty / move
//! rects, pointer metadata). Known limits from the Windows docs that shape
//! the host design:
//!
//! * only a LOCAL_SYSTEM process can duplicate the secure desktop (UAC,
//!   login screen), so the host runs its capture helper as SYSTEM;
//! * duplication is lost on desktop switch / mode change and must be
//!   recreated (`Error::AccessLost`);
//! * the D3D device must live on the adapter that drives the output, which
//!   is why hybrid iGPU/dGPU laptops get the Windows.Graphics.Capture
//!   fallback (not implemented yet).

use std::time::{Duration, Instant};

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows_capture::dxgi_duplication_api::{
    DxgiDuplicationApi, DxgiDuplicationFormat, DxgiDuplicationFrame, Error as DupError,
};
pub use windows_capture::monitor::Monitor;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// No new frame within the timeout. Not an error for the caller; the
    /// desktop simply did not change.
    #[error("no frame within timeout")]
    Timeout,
    /// Desktop switch, mode change or another duplicator took over; the
    /// caller must drop this capturer and build a new one.
    #[error("desktop duplication access lost")]
    AccessLost,
    #[error(transparent)]
    Windows(#[from] windows::core::Error),
    #[error("{0}")]
    Other(String),
}

impl From<DupError> for CaptureError {
    fn from(e: DupError) -> Self {
        match e {
            DupError::Timeout => CaptureError::Timeout,
            DupError::AccessLost => CaptureError::AccessLost,
            other => CaptureError::Other(other.to_string()),
        }
    }
}

/// One captured desktop frame. Lives on the GPU; borrows the capturer, so it
/// must be dropped (which releases the DXGI frame) before the next acquire.
pub struct Frame<'a> {
    inner: DxgiDuplicationFrame<'a>,
    dirty: Vec<RECT>,
    /// Time `acquire` returned, used by the bench tool for pipeline latency.
    pub captured_at: Instant,
}

impl<'a> Frame<'a> {
    pub fn width(&self) -> u32 {
        self.inner.width()
    }
    pub fn height(&self) -> u32 {
        self.inner.height()
    }
    /// BGRA8 (or 10-bit / FP16 for HDR outputs) texture on the capture device.
    pub fn texture(&self) -> &ID3D11Texture2D {
        self.inner.texture()
    }
    pub fn device(&self) -> &ID3D11Device {
        self.inner.device()
    }
    pub fn device_context(&self) -> &ID3D11DeviceContext {
        self.inner.device_context()
    }
    /// Regions that changed since the previous frame. Empty when the whole
    /// frame should be treated as dirty (metadata unavailable).
    pub fn dirty_rects(&self) -> &[RECT] {
        &self.dirty
    }
    /// Number of desktop updates coalesced into this frame (>1 means we
    /// are capturing slower than the desktop is updating).
    pub fn accumulated_frames(&self) -> u32 {
        self.inner.frame_info().AccumulatedFrames
    }
    /// Copy the frame to CPU memory. Slow path: only for tests and the
    /// software-encoder fallback.
    pub fn to_cpu_buffer(&mut self) -> Result<Vec<u8>, CaptureError> {
        let buf = self.inner.buffer().map_err(CaptureError::from)?;
        // Strip row padding so consumers get tightly packed width*height*4.
        let mut scratch = Vec::new();
        Ok(buf.as_nopadding_buffer(&mut scratch).to_vec())
    }
}

/// Desktop Duplication capturer for one monitor.
pub struct Capturer {
    api: DxgiDuplicationApi,
    monitor: Monitor,
}

impl Capturer {
    pub fn new(monitor: Monitor) -> Result<Self, CaptureError> {
        // Force 8-bit BGRA so the encoder input path is uniform; HDR
        // outputs get tone-mapped by DWM for us. Native 10-bit is a later
        // feature.
        let api = DxgiDuplicationApi::new_options(monitor, &[DxgiDuplicationFormat::Bgra8])?;
        tracing::info!(
            width = api.width(),
            height = api.height(),
            format = ?api.format(),
            refresh = ?api.refresh_rate(),
            "desktop duplication started"
        );
        Ok(Self { api, monitor })
    }

    pub fn primary() -> Result<Self, CaptureError> {
        let monitor = Monitor::primary().map_err(|e| CaptureError::Other(e.to_string()))?;
        Self::new(monitor)
    }

    /// All displays, ordered by the OS monitor index.
    pub fn monitors() -> Vec<Monitor> {
        Monitor::enumerate().unwrap_or_default()
    }

    /// Capturer for the display at `index` (clamped to the available range).
    pub fn at_index(index: usize) -> Result<(Self, usize), CaptureError> {
        let all = Self::monitors();
        if all.is_empty() {
            return Self::primary().map(|c| (c, 0));
        }
        let i = index.min(all.len() - 1);
        Self::new(all[i]).map(|c| (c, i))
    }

    pub fn monitor(&self) -> Monitor {
        self.monitor
    }
    pub fn width(&self) -> u32 {
        self.api.width()
    }
    pub fn height(&self) -> u32 {
        self.api.height()
    }
    pub fn device(&self) -> &ID3D11Device {
        self.api.device()
    }

    /// Wait up to `timeout` for the next desktop update.
    pub fn acquire(&mut self, timeout: Duration) -> Result<Frame<'_>, CaptureError> {
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let inner = self.api.acquire_next_frame(timeout_ms)?;
        let captured_at = Instant::now();
        let dirty = dirty_rects(&inner).unwrap_or_default();
        Ok(Frame {
            inner,
            dirty,
            captured_at,
        })
    }

    /// Rebuild after `AccessLost`. Consumes self because the underlying
    /// duplication object is gone.
    pub fn recreate(self) -> Result<Self, CaptureError> {
        let monitor = self.monitor;
        let api = self.api.recreate_options(&[DxgiDuplicationFormat::Bgra8])?;
        Ok(Self { api, monitor })
    }
}

/// Query dirty rectangles for the current frame from the duplication object.
fn dirty_rects(frame: &DxgiDuplicationFrame<'_>) -> Result<Vec<RECT>, CaptureError> {
    let info = frame.frame_info();
    if info.TotalMetadataBufferSize == 0 {
        return Ok(Vec::new());
    }
    let capacity = (info.TotalMetadataBufferSize as usize / std::mem::size_of::<RECT>()).max(1);
    let mut rects: Vec<RECT> = Vec::with_capacity(capacity);
    let mut required: u32 = 0;
    // SAFETY: buffer has `capacity` RECTs of spare room; DXGI writes at most
    // that many bytes and reports how many it needed.
    unsafe {
        frame.duplication().GetFrameDirtyRects(
            (capacity * std::mem::size_of::<RECT>()) as u32,
            rects.as_mut_ptr(),
            &mut required,
        )?;
        rects.set_len(required as usize / std::mem::size_of::<RECT>());
    }
    Ok(rects)
}
