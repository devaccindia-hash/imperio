//! WASAPI shared-mode playback on the viewer's default render endpoint. Like
//! capture, the COM objects live on a dedicated thread; decoded 20 ms frames
//! are submitted over a channel and the thread paces them into the device
//! buffer, tolerating small timing jitter.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

use crate::{i16_to_f32, CHANNELS, INTERLEAVED_PER_FRAME, SAMPLE_RATE};

/// A running playback sink. Dropping it stops the thread.
pub struct AudioPlayback {
    tx: SyncSender<Vec<i16>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioPlayback {
    pub fn start() -> anyhow::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = sync_channel::<Vec<i16>>(50);
        let stop_thread = stop.clone();
        let handle = std::thread::Builder::new()
            .name("audio-playback".into())
            .spawn(move || {
                if let Err(e) = playback_loop(&rx, &stop_thread) {
                    tracing::warn!(error = %e, "audio playback stopped");
                }
            })?;
        Ok(Self {
            tx,
            stop,
            handle: Some(handle),
        })
    }

    /// Queue a decoded 20 ms frame (interleaved i16 stereo) for playback.
    /// Drops the frame if the buffer is full (sink fell behind).
    pub fn submit(&self, frame: Vec<i16>) {
        let _ = self.tx.try_send(frame);
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn playback_loop(rx: &Receiver<Vec<i16>>, stop: &AtomicBool) -> anyhow::Result<()> {
    // SAFETY: standard WASAPI shared-mode render bring-up; pointers are used
    // only while their COM object is alive and within the GetBuffer/
    // ReleaseBuffer contract.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let result = (|| -> anyhow::Result<()> {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let wf = client.GetMixFormat()?;
            let (dst_ch, dst_rate, is_float) = {
                let f = &*wf;
                (f.nChannels, f.nSamplesPerSec, f.wBitsPerSample == 32)
            };
            if dst_rate != SAMPLE_RATE {
                tracing::warn!(dst_rate, want = SAMPLE_RATE, "render mix rate differs; no resampling yet");
            }
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, wf, None)?;
            CoTaskMemFree(Some(wf as *const _));
            let render: IAudioRenderClient = client.GetService()?;
            let buffer_frames = client.GetBufferSize()?;
            client.Start()?;

            // Small jitter buffer of interleaved stereo samples.
            let mut queue: VecDeque<i16> = VecDeque::with_capacity(INTERLEAVED_PER_FRAME * 8);
            let mut floats: Vec<f32> = Vec::new();
            let poll = Duration::from_millis(5);

            while !stop.load(Ordering::Relaxed) {
                // Drain submitted frames into the jitter buffer.
                while let Ok(frame) = rx.try_recv() {
                    queue.extend(frame);
                }
                let padding = client.GetCurrentPadding()?;
                let avail = buffer_frames.saturating_sub(padding);
                let have_frames = (queue.len() / dst_ch.max(1) as usize) as u32;
                let to_write = avail.min(have_frames);
                if to_write == 0 {
                    std::thread::sleep(poll);
                    continue;
                }
                let samples = to_write as usize * dst_ch as usize;
                let data: *mut u8 = render.GetBuffer(to_write)?;
                if !data.is_null() {
                    write_samples(&mut queue, samples, dst_ch, is_float, data, &mut floats);
                }
                render.ReleaseBuffer(to_write, 0)?;
            }
            let _ = client.Stop();
            Ok(())
        })();
        CoUninitialize();
        result
    }
}

/// Pop `samples` interleaved stereo values from `queue`, expand to the device
/// channel count, and write them (float or i16) into the mapped buffer.
unsafe fn write_samples(
    queue: &mut VecDeque<i16>,
    samples: usize,
    dst_ch: u16,
    is_float: bool,
    data: *mut u8,
    floats: &mut Vec<f32>,
) {
    // Pull stereo samples (2 per frame) and map to dst_ch channels.
    let frames = samples / dst_ch as usize;
    let mut stereo: Vec<i16> = Vec::with_capacity(frames * CHANNELS as usize);
    for _ in 0..frames {
        let l = queue.pop_front().unwrap_or(0);
        let r = queue.pop_front().unwrap_or(0);
        match dst_ch {
            1 => stereo.push(((l as i32 + r as i32) / 2) as i16),
            2 => {
                stereo.push(l);
                stereo.push(r);
            }
            n => {
                stereo.push(l);
                stereo.push(r);
                stereo.extend(std::iter::repeat_n(0, n as usize - 2));
            }
        }
    }
    if is_float {
        i16_to_f32(&stereo, floats);
        std::ptr::copy_nonoverlapping(floats.as_ptr() as *const u8, data, floats.len() * 4);
    } else {
        std::ptr::copy_nonoverlapping(stereo.as_ptr() as *const u8, data, stereo.len() * 2);
    }
}
