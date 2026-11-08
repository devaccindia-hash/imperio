//! Session recording: write the received H.264 stream into an `.mp4`.
//!
//! No re-encoding: the compressed access units the viewer already receives are
//! handed to the Media Foundation sink writer with an H.264 input type equal to
//! the output type, so it only muxes. Recording starts at the first keyframe
//! (the viewer requests one), timestamps are rebased to zero and kept
//! monotonic, and `Finalize` writes the MP4 index on stop or drop.

use std::path::{Path, PathBuf};

use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::{
    IMFSinkWriter, MFCreateAttributes, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFCreateSinkWriterFromURL, MFMediaType_Video, MFSampleExtension_CleanPoint,
    MFVideoFormat_H264, MFVideoInterlace_Progressive, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SINK_WRITER_DISABLE_THROTTLING,
};

pub struct Mp4Recorder {
    writer: IMFSinkWriter,
    stream: u32,
    start_pts: Option<i64>,
    last_ts: i64,
    frame_dur: i64,
    frames: u64,
    finished: bool,
    pub path: PathBuf,
}

impl Mp4Recorder {
    /// Open `path` for writing a `width` x `height` H.264 stream at `fps`.
    pub fn create(path: &Path, width: u32, height: u32, fps: u32) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // SAFETY: standard sink-writer bring-up; every COM object is owned by
        // this struct or dropped at the end of the call.
        unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 1)?;
            let attrs = attrs.expect("attributes");
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            let url = HSTRING::from(path.as_os_str());
            let writer = MFCreateSinkWriterFromURL(&url, None, Some(&attrs))?;
            let ty = MFCreateMediaType()?;
            ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            ty.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            ty.SetUINT64(&MF_MT_FRAME_RATE, ((fps.max(1) as u64) << 32) | 1)?;
            ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            ty.SetUINT32(&MF_MT_AVG_BITRATE, 8_000_000)?;
            let stream = writer.AddStream(&ty)?;
            writer.SetInputMediaType(stream, &ty, None)?;
            writer.BeginWriting()?;
            Ok(Self {
                writer,
                stream,
                start_pts: None,
                last_ts: -1,
                frame_dur: 10_000_000 / fps.max(1) as i64,
                frames: 0,
                finished: false,
                path: path.to_path_buf(),
            })
        }
    }

    /// Append one access unit. Frames before the first keyframe are skipped.
    pub fn write(&mut self, data: &[u8], keyframe: bool, pts_100ns: i64) -> anyhow::Result<()> {
        let start = match self.start_pts {
            Some(s) => s,
            None => {
                if !keyframe {
                    return Ok(());
                }
                self.start_pts = Some(pts_100ns);
                pts_100ns
            }
        };
        let mut ts = pts_100ns - start;
        if ts <= self.last_ts {
            ts = self.last_ts + self.frame_dur;
        }
        // SAFETY: the buffer is locked, filled within its allocated length and
        // unlocked before use; the sample owns it afterwards.
        unsafe {
            let buf = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut max_len = 0u32;
            buf.Lock(&mut ptr, Some(&mut max_len), None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buf.Unlock()?;
            buf.SetCurrentLength(data.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(ts)?;
            sample.SetSampleDuration(self.frame_dur)?;
            if keyframe {
                sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
            }
            self.writer.WriteSample(self.stream, &sample)?;
        }
        self.last_ts = ts;
        self.frames += 1;
        Ok(())
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Finish the file. Returns the path and frame count.
    pub fn finish(mut self) -> anyhow::Result<(PathBuf, u64)> {
        self.finalize_inner()?;
        Ok((self.path.clone(), self.frames))
    }

    fn finalize_inner(&mut self) -> anyhow::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        // SAFETY: Finalize is the documented last call on a sink writer.
        unsafe { self.writer.Finalize()? };
        Ok(())
    }
}

impl Drop for Mp4Recorder {
    fn drop(&mut self) {
        let _ = self.finalize_inner();
    }
}

/// `%USERPROFILE%\Videos\Imperio\Imperio-YYYY-MM-DD_HHMMSS.mp4` (UTC clock).
pub fn default_recording_path() -> PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Videos")
        .join("Imperio");
    base.join(format!("Imperio-{}.mp4", utc_stamp()))
}

fn utc_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil-from-days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}_{hh:02}{mm:02}{ss:02}")
}
