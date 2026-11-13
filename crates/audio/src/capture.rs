//! WASAPI loopback capture of the host's default render endpoint (i.e. whatever
//! the host is playing). Runs the COM objects on a dedicated thread, because
//! WASAPI interfaces are bound to the thread's COM apartment and are not
//! `Send`. The thread converts the shared mix format to interleaved 16-bit
//! stereo and emits fixed 20 ms frames over a channel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

use crate::{f32_to_i16, CHANNELS, INTERLEAVED_PER_FRAME, SAMPLE_RATE};

/// A running loopback capture. Dropping it stops the thread.
pub struct AudioCapture {
    rx: Receiver<Vec<i16>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioCapture {
    /// Start capturing system audio. Returns immediately; frames arrive on the
    /// internal channel and are read with [`try_recv`](Self::try_recv).
    pub fn start() -> anyhow::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        // Bounded so a stalled reader drops old audio instead of growing
        // unboundedly; ~0.5 s at 20 ms frames.
        let (tx, rx) = sync_channel::<Vec<i16>>(25);
        let stop_thread = stop.clone();
        let handle = std::thread::Builder::new()
            .name("audio-capture".into())
            .spawn(move || {
                if let Err(e) = capture_loop(&tx, &stop_thread) {
                    tracing::warn!(error = %e, "audio capture stopped");
                }
            })?;
        Ok(Self {
            rx,
            stop,
            handle: Some(handle),
        })
    }

    /// Next captured 20 ms frame (interleaved i16 stereo), if one is ready.
    pub fn try_recv(&self) -> Option<Vec<i16>> {
        self.rx.try_recv().ok()
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Read a `WAVEFORMATEX` and report (channels, sample_rate, is_float).
unsafe fn format_of(wf: *const WAVEFORMATEX) -> (u16, u32, bool) {
    let f = &*wf;
    let is_float = f.wBitsPerSample == 32;
    (f.nChannels, f.nSamplesPerSec, is_float)
}

fn capture_loop(tx: &SyncSender<Vec<i16>>, stop: &AtomicBool) -> anyhow::Result<()> {
    // SAFETY: standard WASAPI shared-mode loopback bring-up; every raw pointer
    // is used only while the owning COM object is alive and within its
    // documented contract.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let result = (|| -> anyhow::Result<()> {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let wf = client.GetMixFormat()?;
            let (src_ch, src_rate, is_float) = format_of(wf);
            if src_rate != SAMPLE_RATE {
                tracing::warn!(
                    src_rate,
                    want = SAMPLE_RATE,
                    "capture mix rate differs; no resampling yet, pitch may be wrong"
                );
            }
            // 100 ms shared buffer; loopback cannot be event-driven, so we poll.
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                1_000_000,
                0,
                wf,
                None,
            )?;
            let capture: IAudioCaptureClient = client.GetService()?;
            CoTaskMemFree(Some(wf as *const _));
            client.Start()?;

            let mut pending: Vec<i16> = Vec::with_capacity(INTERLEAVED_PER_FRAME * 2);
            let mut i16s: Vec<i16> = Vec::new();
            let poll = Duration::from_millis(5);

            while !stop.load(Ordering::Relaxed) {
                let mut packet = capture.GetNextPacketSize()?;
                if packet == 0 {
                    std::thread::sleep(poll);
                    continue;
                }
                while packet != 0 {
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut frames: u32 = 0;
                    let mut flags: u32 = 0;
                    capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                    let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                    let sample_count = frames as usize * src_ch as usize;
                    if silent || data.is_null() {
                        // Emit silence so the viewer's clock keeps advancing.
                        pending.extend(std::iter::repeat_n(0i16, frames as usize * CHANNELS as usize));
                    } else if is_float {
                        let src = std::slice::from_raw_parts(data as *const f32, sample_count);
                        f32_to_i16(src, &mut i16s);
                        downmix_to_stereo(&i16s, src_ch, &mut pending);
                    } else {
                        let src = std::slice::from_raw_parts(data as *const i16, sample_count);
                        downmix_to_stereo(src, src_ch, &mut pending);
                    }
                    capture.ReleaseBuffer(frames)?;

                    while pending.len() >= INTERLEAVED_PER_FRAME {
                        let frame: Vec<i16> = pending.drain(..INTERLEAVED_PER_FRAME).collect();
                        match tx.try_send(frame) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {} // reader behind; drop
                            Err(TrySendError::Disconnected(_)) => return Ok(()),
                        }
                    }
                    packet = capture.GetNextPacketSize()?;
                }
            }
            let _ = client.Stop();
            Ok(())
        })();
        CoUninitialize();
        result
    }
}

/// Collapse `src` (interleaved, `src_ch` channels) to interleaved stereo and
/// append to `out`. Mono is duplicated; more than two channels keeps the
/// first two (front L/R).
fn downmix_to_stereo(src: &[i16], src_ch: u16, out: &mut Vec<i16>) {
    match src_ch {
        0 => {}
        1 => {
            for &s in src {
                out.push(s);
                out.push(s);
            }
        }
        2 => out.extend_from_slice(src),
        n => {
            let n = n as usize;
            for frame in src.chunks_exact(n) {
                out.push(frame[0]);
                out.push(frame[1]);
            }
        }
    }
}
