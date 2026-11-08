//! Media Foundation H.264 encoder / decoder backends.

use std::mem::ManuallyDrop;
use std::time::Instant;

use anyhow::{anyhow, Context};
use bytes::Bytes;
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;

use crate::d3d::VideoDevice;
use crate::{DecodedFrame, EncodedFrame, EncoderConfig, VideoDecoder, VideoEncoder};

const MF_E_TRANSFORM_NEED_MORE_INPUT: i32 = 0xC00D6D72u32 as i32;
const MF_E_TRANSFORM_STREAM_CHANGE: i32 = 0xC00D6D61u32 as i32;
const MF_E_NO_EVENTS_AVAILABLE: i32 = 0xC00D3E80u32 as i32;

/// Must be called once per process before any other MF call.
pub fn startup() -> anyhow::Result<()> {
    let version = (MF_SDK_VERSION << 16) | MF_API_VERSION;
    unsafe { MFStartup(version, MFSTARTUP_FULL) }.context("MFStartup")?;
    Ok(())
}

fn pack_u64(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut ptr = PWSTR::null();
    let mut len = 0u32;
    unsafe {
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut ptr, &mut len)
            .is_ok()
            && !ptr.is_null()
        {
            let s = ptr.to_string().unwrap_or_default();
            CoTaskMemFree(Some(ptr.as_ptr() as *const _));
            return s;
        }
    }
    "<unnamed MFT>".into()
}

fn enum_transforms(
    category: GUID,
    flags: MFT_ENUM_FLAG,
    input: Option<&MFT_REGISTER_TYPE_INFO>,
    output: Option<&MFT_REGISTER_TYPE_INFO>,
) -> anyhow::Result<Vec<IMFActivate>> {
    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            category,
            flags,
            input.map(|p| p as *const _),
            output.map(|p| p as *const _),
            &mut ptr,
            &mut count,
        )
        .context("MFTEnumEx")?;
    }
    let mut out = Vec::with_capacity(count as usize);
    if !ptr.is_null() {
        for i in 0..count as usize {
            // SAFETY: MFTEnumEx returned `count` initialised entries.
            if let Some(a) = unsafe { (*ptr.add(i)).take() } {
                out.push(a);
            }
        }
        unsafe { CoTaskMemFree(Some(ptr as *const _)) };
    }
    Ok(out)
}

/// Shared transform driver: handles sync and async (hardware) MFTs.
struct Transform {
    mft: IMFTransform,
    is_async: bool,
    events: Option<IMFMediaEventGenerator>,
    /// Pending METransformNeedInput credits (async only).
    input_credits: u32,
    provides_samples: bool,
    output_size: u32,
    /// Subtype to re-select after MF_E_TRANSFORM_STREAM_CHANGE.
    output_subtype: GUID,
}

impl Transform {
    fn new(
        mft: IMFTransform,
        device: Option<&VideoDevice>,
        output_subtype: GUID,
    ) -> anyhow::Result<Self> {
        let attrs = unsafe { mft.GetAttributes() }.ok();
        let mut is_async = false;
        if let Some(attrs) = &attrs {
            is_async = unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) == 1;
            if is_async {
                unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                    .context("async unlock")?;
            }
            let _ = unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) };
            let d3d_aware = unsafe { attrs.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) == 1;
            if let (true, Some(dev)) = (d3d_aware, device) {
                let mut token = 0u32;
                let mut manager: Option<IMFDXGIDeviceManager> = None;
                unsafe {
                    MFCreateDXGIDeviceManager(&mut token, &mut manager)
                        .context("MFCreateDXGIDeviceManager")?;
                    let manager = manager.expect("manager");
                    manager
                        .ResetDevice(&dev.device, token)
                        .context("ResetDevice")?;
                    mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                        .context("SET_D3D_MANAGER")?;
                    // MF keeps its own reference; ours can go.
                    std::mem::forget(manager);
                }
                tracing::info!("transform bound to D3D11 device");
            } else if device.is_some() {
                tracing::warn!(
                    "transform is not D3D11-aware; frames will go through system memory"
                );
            }
        }
        let events = if is_async {
            Some(mft.cast::<IMFMediaEventGenerator>()?)
        } else {
            None
        };
        Ok(Self {
            mft,
            is_async,
            events,
            input_credits: 0,
            provides_samples: false,
            output_size: 0,
            output_subtype,
        })
    }

    fn codec_api(&self) -> Option<ICodecAPI> {
        self.mft.cast::<ICodecAPI>().ok()
    }

    fn start(&mut self) -> anyhow::Result<()> {
        unsafe {
            let info = self
                .mft
                .GetOutputStreamInfo(0)
                .context("GetOutputStreamInfo")?;
            self.provides_samples = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                    as u32
                != 0;
            self.output_size = info.cbSize.max(1 << 20);
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("BEGIN_STREAMING")?;
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("START_OF_STREAM")?;
        }
        Ok(())
    }

    /// Pump async events. `wait` blocks until at least one arrives.
    /// Returns output samples drained along the way.
    fn pump(&mut self, wait: bool) -> anyhow::Result<Vec<IMFSample>> {
        let Some(gen) = self.events.clone() else {
            return Ok(Vec::new());
        };
        let mut outputs = Vec::new();
        let mut first = true;
        loop {
            let flags = if wait && first {
                MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)
            } else {
                MF_EVENT_FLAG_NO_WAIT
            };
            first = false;
            let ev = match unsafe { gen.GetEvent(flags) } {
                Ok(ev) => ev,
                Err(e) if e.code().0 == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(e) => return Err(e).context("GetEvent"),
            };
            let ty = unsafe { ev.GetType() }.context("GetType")?;
            if ty == METransformNeedInput.0 as u32 {
                self.input_credits += 1;
            } else if ty == METransformHaveOutput.0 as u32 {
                if let Some(s) = self.process_output()? {
                    outputs.push(s);
                }
            }
        }
        Ok(outputs)
    }

    fn process_input(&mut self, sample: &IMFSample) -> anyhow::Result<Vec<IMFSample>> {
        let mut outputs = Vec::new();
        if self.is_async {
            while self.input_credits == 0 {
                outputs.extend(self.pump(true)?);
            }
            self.input_credits -= 1;
            unsafe { self.mft.ProcessInput(0, sample, 0) }.context("ProcessInput")?;
            outputs.extend(self.pump(false)?);
        } else {
            unsafe { self.mft.ProcessInput(0, sample, 0) }.context("ProcessInput")?;
            while let Some(s) = self.process_output()? {
                outputs.push(s);
            }
        }
        Ok(outputs)
    }

    /// One ProcessOutput call. `None` when the MFT needs more input.
    fn process_output(&mut self) -> anyhow::Result<Option<IMFSample>> {
        let sample = if self.provides_samples {
            None
        } else {
            unsafe {
                let buffer =
                    MFCreateMemoryBuffer(self.output_size).context("MFCreateMemoryBuffer")?;
                let s = MFCreateSample().context("MFCreateSample")?;
                s.AddBuffer(&buffer).context("AddBuffer")?;
                Some(s)
            }
        };
        let mut out = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        let hr = unsafe {
            self.mft
                .ProcessOutput(0, std::slice::from_mut(&mut out), &mut status)
        };
        // SAFETY: we own both fields; take them back so they are released.
        let sample = unsafe { ManuallyDrop::take(&mut out.pSample) };
        let _events = unsafe { ManuallyDrop::take(&mut out.pEvents) };
        match hr {
            Ok(()) => Ok(sample),
            Err(e) if e.code().0 == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
            Err(e) if e.code().0 == MF_E_TRANSFORM_STREAM_CHANGE => {
                self.renegotiate_output()?;
                if self.is_async {
                    // The MFT will raise a new METransformHaveOutput; calling
                    // ProcessOutput before that is E_UNEXPECTED.
                    Ok(None)
                } else {
                    self.process_output()
                }
            }
            Err(e) => Err(e).context("ProcessOutput"),
        }
    }

    /// Re-select the output type with our subtype after a stream change.
    /// Decoders use this to pick NV12; hardware encoders raise it once at
    /// start-up and simply want the H.264 type re-applied.
    fn renegotiate_output(&mut self) -> anyhow::Result<()> {
        let mut i = 0;
        loop {
            let ty = match unsafe { self.mft.GetOutputAvailableType(0, i) } {
                Ok(ty) => ty,
                Err(e) => {
                    // Some encoders enumerate nothing; fall back to re-applying
                    // the type that is already current.
                    if i == 0 {
                        if let Ok(cur) = unsafe { self.mft.GetOutputCurrentType(0) } {
                            unsafe { self.mft.SetOutputType(0, &cur, 0) }
                                .context("SetOutputType(current)")?;
                            return self.refresh_stream_info();
                        }
                    }
                    return Err(e).context("GetOutputAvailableType");
                }
            };
            let sub = unsafe { ty.GetGUID(&MF_MT_SUBTYPE) }?;
            if sub == self.output_subtype {
                unsafe { self.mft.SetOutputType(0, &ty, 0) }.context("SetOutputType")?;
                return self.refresh_stream_info();
            }
            i += 1;
        }
    }

    fn refresh_stream_info(&mut self) -> anyhow::Result<()> {
        let info = unsafe { self.mft.GetOutputStreamInfo(0) }?;
        self.provides_samples = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0;
        self.output_size = info.cbSize.max(1 << 20);
        Ok(())
    }
}

fn set_codec_u32(api: &Option<ICodecAPI>, guid: &GUID, value: u32, what: &str) {
    if let Some(api) = api {
        let v = VARIANT::from(value);
        if let Err(e) = unsafe { api.SetValue(guid, &v) } {
            tracing::debug!(%what, error = %e, "ICodecAPI SetValue rejected");
        }
    }
}

fn set_codec_bool(api: &Option<ICodecAPI>, guid: &GUID, value: bool, what: &str) {
    if let Some(api) = api {
        let v = VARIANT::from(value);
        if let Err(e) = unsafe { api.SetValue(guid, &v) } {
            tracing::debug!(%what, error = %e, "ICodecAPI SetValue rejected");
        }
    }
}

/// H.264 encoder on top of an MF transform.
pub struct MfH264Encoder {
    t: Transform,
    api: Option<ICodecAPI>,
    cfg: EncoderConfig,
    name: String,
    frame_duration: i64,
}

impl MfH264Encoder {
    /// Try hardware encoders first, then Microsoft's software encoder.
    pub fn new(dev: &VideoDevice, cfg: EncoderConfig) -> anyhow::Result<Self> {
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let mut candidates = enum_transforms(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
        )?;
        let hw_count = candidates.len();
        candidates.extend(enum_transforms(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
        )?);

        // (name, activate, is_hardware) with a selection priority. Hardware
        // first, but among hardware prefer encoders measured to add the least
        // latency here (Intel Quick Sync ~4.5 ms) over ones whose MF transform
        // buffers a deep pipeline by default (NVENC, until low-latency tuning
        // lands). `IMPERIO_ENCODER=<substring>` forces a specific encoder.
        let force = std::env::var("IMPERIO_ENCODER").ok().filter(|s| !s.is_empty());
        let mut ranked: Vec<(String, &IMFActivate, bool)> = candidates
            .iter()
            .enumerate()
            .map(|(i, act)| (friendly_name(act), act, i < hw_count))
            .collect();
        ranked.sort_by_key(|(name, _, is_hw)| {
            let n = name.to_lowercase();
            let forced = force.as_ref().is_some_and(|f| n.contains(&f.to_lowercase()));
            let family = if n.contains("quick sync") || n.contains("intel") {
                0
            } else if n.contains("nvidia") {
                2
            } else {
                1
            };
            (!forced, !*is_hw, family)
        });
        tracing::info!(
            hardware = hw_count,
            total = ranked.len(),
            order = ?ranked.iter().map(|(n, _, _)| n.as_str()).collect::<Vec<_>>(),
            "H.264 encoder candidates"
        );

        let mut last_err = anyhow!("no H.264 encoder MFT found");
        for (name, act, is_hw) in &ranked {
            let (name, is_hw) = (name.clone(), *is_hw);
            match Self::try_create(act, &name, is_hw, dev, cfg.clone()) {
                Ok(enc) => {
                    tracing::info!(%name, hardware = is_hw, "encoder ready");
                    return Ok(enc);
                }
                Err(e) => {
                    tracing::warn!(%name, error = format_args!("{e:#}"), "encoder candidate failed");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    fn try_create(
        act: &IMFActivate,
        name: &str,
        is_hw: bool,
        dev: &VideoDevice,
        cfg: EncoderConfig,
    ) -> anyhow::Result<Self> {
        let mft: IMFTransform = unsafe { act.ActivateObject() }.context("ActivateObject")?;
        let mut t = Transform::new(mft, is_hw.then_some(dev), MFVideoFormat_H264)?;
        let api = t.codec_api();

        // Low latency, CBR, no B-frames, long GOP. Rejections are logged
        // at debug level: not every encoder supports every knob.
        set_codec_bool(&api, &CODECAPI_AVLowLatencyMode, true, "LowLatencyMode");
        set_codec_u32(
            &api,
            &CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_CBR.0 as u32,
            "RateControlMode",
        );
        set_codec_u32(
            &api,
            &CODECAPI_AVEncCommonMeanBitRate,
            cfg.bitrate_kbps * 1000,
            "MeanBitRate",
        );
        set_codec_u32(&api, &CODECAPI_AVEncMPVGOPSize, cfg.gop_frames, "GOPSize");
        set_codec_u32(
            &api,
            &CODECAPI_AVEncMPVDefaultBPictureCount,
            0,
            "BPictureCount",
        );
        set_codec_u32(
            &api,
            &CODECAPI_AVEncCommonQualityVsSpeed,
            33,
            "QualityVsSpeed",
        );

        unsafe {
            let out_ty = MFCreateMediaType()?;
            out_ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_ty.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_kbps * 1000)?;
            out_ty.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(cfg.width, cfg.height))?;
            out_ty.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(cfg.fps, 1))?;
            out_ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out_ty.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            out_ty.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 0)?;
            t.mft
                .SetOutputType(0, &out_ty, 0)
                .context("SetOutputType(H264)")?;

            let in_ty = MFCreateMediaType()?;
            in_ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_ty.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(cfg.width, cfg.height))?;
            in_ty.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(cfg.fps, 1))?;
            in_ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            in_ty.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            t.mft
                .SetInputType(0, &in_ty, 0)
                .context("SetInputType(NV12)")?;
        }
        t.start()?;
        let frame_duration = 10_000_000 / cfg.fps.max(1) as i64;
        Ok(Self {
            t,
            api,
            cfg,
            name: name.to_string(),
            frame_duration,
        })
    }

    fn sample_to_frame(
        &self,
        sample: &IMFSample,
        started: Instant,
    ) -> anyhow::Result<EncodedFrame> {
        unsafe {
            let buf = sample
                .ConvertToContiguousBuffer()
                .context("ConvertToContiguousBuffer")?;
            let mut ptr = std::ptr::null_mut();
            let mut max = 0u32;
            let mut cur = 0u32;
            buf.Lock(&mut ptr, Some(&mut max), Some(&mut cur))
                .context("Lock")?;
            let data = Bytes::copy_from_slice(std::slice::from_raw_parts(ptr, cur as usize));
            buf.Unlock().context("Unlock")?;
            let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
            let pts = sample.GetSampleTime().unwrap_or(0);
            Ok(EncodedFrame {
                codec: self.cfg.codec,
                data,
                keyframe,
                pts_100ns: pts,
                encode_time: started.elapsed(),
            })
        }
    }
}

impl VideoEncoder for MfH264Encoder {
    fn encode(
        &mut self,
        nv12: &ID3D11Texture2D,
        pts_100ns: i64,
        force_keyframe: bool,
    ) -> anyhow::Result<Vec<EncodedFrame>> {
        let started = Instant::now();
        if force_keyframe {
            set_codec_u32(
                &self.api,
                &CODECAPI_AVEncVideoForceKeyFrame,
                1,
                "ForceKeyFrame",
            );
        }
        let sample = unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false)
                .context("MFCreateDXGISurfaceBuffer")?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                let len = b2.GetContiguousLength().unwrap_or(0);
                let _ = buffer.SetCurrentLength(len);
            }
            let s = MFCreateSample()?;
            s.AddBuffer(&buffer)?;
            s.SetSampleTime(pts_100ns)?;
            s.SetSampleDuration(self.frame_duration)?;
            s
        };
        let outputs = self.t.process_input(&sample)?;
        outputs
            .iter()
            .map(|s| self.sample_to_frame(s, started))
            .collect()
    }

    fn set_bitrate(&mut self, kbps: u32) -> anyhow::Result<()> {
        let Some(api) = &self.api else {
            anyhow::bail!("encoder exposes no ICodecAPI")
        };
        let v = VARIANT::from(kbps * 1000);
        unsafe { api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &v) }.context("set MeanBitRate")?;
        self.cfg.bitrate_kbps = kbps;
        Ok(())
    }

    fn config(&self) -> &EncoderConfig {
        &self.cfg
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// H.264 decoder producing NV12 D3D11 textures.
pub struct MfH264Decoder {
    t: Transform,
    name: String,
    width: u32,
    height: u32,
}

impl MfH264Decoder {
    pub fn new(dev: &VideoDevice, width: u32, height: u32, fps: u32) -> anyhow::Result<Self> {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let candidates = enum_transforms(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            None,
        )?;
        let act = candidates
            .first()
            .ok_or_else(|| anyhow!("no H.264 decoder MFT"))?;
        let name = friendly_name(act);
        let mft: IMFTransform = unsafe { act.ActivateObject() }.context("ActivateObject")?;
        let mut t = Transform::new(mft, Some(dev), MFVideoFormat_NV12)?;
        let api = t.codec_api();
        set_codec_bool(&api, &CODECAPI_AVLowLatencyMode, true, "LowLatencyMode");

        unsafe {
            let in_ty = MFCreateMediaType()?;
            in_ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_ty.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            in_ty.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(width, height))?;
            in_ty.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
            in_ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            t.mft
                .SetInputType(0, &in_ty, 0)
                .context("SetInputType(H264)")?;
        }
        t.renegotiate_output()?;
        t.start()?;
        tracing::info!(%name, "decoder ready");
        Ok(Self {
            t,
            name,
            width,
            height,
        })
    }
}

impl VideoDecoder for MfH264Decoder {
    fn decode(&mut self, data: &[u8], pts_100ns: i64) -> anyhow::Result<Vec<DecodedFrame>> {
        let sample = unsafe {
            let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(data.len() as u32)?;
            let s = MFCreateSample()?;
            s.AddBuffer(&buffer)?;
            s.SetSampleTime(pts_100ns)?;
            s
        };
        let outputs = self.t.process_input(&sample)?;
        let mut frames = Vec::with_capacity(outputs.len());
        for s in outputs {
            unsafe {
                let buf = s.GetBufferByIndex(0).context("GetBufferByIndex")?;
                let dxgi: IMFDXGIBuffer =
                    buf.cast().context("output buffer is not a DXGI buffer")?;
                let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)
                    .context("GetResource")?;
                // SAFETY: GetResource returned an AddRef'd ID3D11Texture2D.
                let texture = ID3D11Texture2D::from_raw(raw);
                let subresource = dxgi.GetSubresourceIndex().unwrap_or(0);
                let pts = s.GetSampleTime().unwrap_or(0);
                frames.push(DecodedFrame {
                    texture,
                    subresource,
                    width: self.width,
                    height: self.height,
                    pts_100ns: pts,
                    _sample: s,
                });
            }
        }
        Ok(frames)
    }

    fn name(&self) -> &str {
        &self.name
    }
}
