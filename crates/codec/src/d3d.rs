//! D3D11 device, cross-device texture sharing and GPU colour conversion.

use std::mem::ManuallyDrop;

use windows::core::{Interface, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, LUID, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory1, IDXGIKeyedMutex, IDXGIResource1,
    DXGI_ADAPTER_DESC, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
};

/// PCI vendor IDs used to recognise an integrated Intel GPU (low-latency
/// Quick Sync encoder) versus a discrete NVIDIA GPU (deep, high-latency
/// Media Foundation NVENC pipeline).
const VENDOR_INTEL: u32 = 0x8086;
const VENDOR_NVIDIA: u32 = 0x10DE;

/// The DXGI adapter backing a D3D11 device.
fn adapter_of(device: &ID3D11Device) -> Result<IDXGIAdapter> {
    let dxgi: IDXGIDevice = device.cast()?;
    unsafe { dxgi.GetAdapter() }
}

/// A D3D11 device created with video support and multithread protection,
/// which Media Foundation requires.
#[derive(Clone)]
pub struct VideoDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub video_device: ID3D11VideoDevice,
    pub video_context: ID3D11VideoContext,
}

impl VideoDevice {
    /// Codec device on the default adapter.
    pub fn new() -> Result<Self> {
        Self::create(None)
    }

    /// Codec device on the same adapter that drives `other` (the capture
    /// device). On hybrid-GPU laptops the display is usually driven by the
    /// integrated GPU; matching its adapter lets the integrated hardware
    /// encoder (e.g. Intel Quick Sync) activate instead of a discrete
    /// encoder whose Media Foundation transform buffers a deep, high-latency
    /// pipeline. Falls back to the default adapter on any failure.
    pub fn matching(other: &ID3D11Device) -> Result<Self> {
        match adapter_of(other).and_then(|a| Self::create(Some(&a))) {
            Ok(v) => Ok(v),
            Err(e) => {
                tracing::warn!(error = %e, "codec device on capture adapter failed; using default");
                Self::create(None)
            }
        }
    }

    /// Codec device on a specific adapter, chosen by `pick_encoder_adapter`.
    /// Unlike `matching`, this does not fall back: the caller decides what to
    /// do when the chosen adapter cannot host a codec device.
    pub fn on_adapter(adapter: &IDXGIAdapter) -> Result<Self> {
        Self::create(Some(adapter))
    }

    fn create(adapter: Option<&IDXGIAdapter>) -> Result<Self> {
        let mut device = None;
        let mut context = None;
        let mut level = D3D_FEATURE_LEVEL::default();
        // With an explicit adapter the driver type must be UNKNOWN.
        let driver: D3D_DRIVER_TYPE = if adapter.is_some() {
            D3D_DRIVER_TYPE_UNKNOWN
        } else {
            D3D_DRIVER_TYPE_HARDWARE
        };
        // SAFETY: standard device creation; all out-pointers are valid.
        unsafe {
            D3D11CreateDevice(
                adapter,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )?;
        }
        let device = device.expect("device");
        let context = context.expect("context");
        // MF transforms touch the device from their own threads.
        let mt: ID3D10Multithread = context.cast()?;
        let _ = unsafe { mt.SetMultithreadProtected(true) };
        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext = context.cast()?;
        tracing::info!(?level, on_adapter = adapter.is_some(), "codec D3D11 device created");
        Ok(Self {
            device,
            context,
            video_device,
            video_context,
        })
    }

    /// Plain GPU texture usable as a video processor output / encoder input.
    pub fn create_texture(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        Ok(tex.expect("texture"))
    }
}

/// A BGRA texture shared between two D3D11 devices with a keyed mutex.
/// Producer: `acquire(0)`, write, `release(1)`. Consumer: `acquire(1)`,
/// read, `release(0)`.
pub struct SharedTexture {
    pub texture: ID3D11Texture2D,
    pub mutex: IDXGIKeyedMutex,
    handle: HANDLE,
    owns_handle: bool,
}

impl SharedTexture {
    pub fn create(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
                | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32,
        };
        let mut tex = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        let texture: ID3D11Texture2D = tex.expect("texture");
        let resource: IDXGIResource1 = texture.cast()?;
        let handle = unsafe {
            resource.CreateSharedHandle(
                None,
                (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0,
                None,
            )?
        };
        let mutex: IDXGIKeyedMutex = texture.cast()?;
        Ok(Self {
            texture,
            mutex,
            handle,
            owns_handle: true,
        })
    }

    /// Open the same texture on another device.
    pub fn open_on(&self, device: &ID3D11Device) -> Result<SharedTexture> {
        let device1: ID3D11Device1 = device.cast()?;
        let texture: ID3D11Texture2D = unsafe { device1.OpenSharedResource1(self.handle)? };
        let mutex: IDXGIKeyedMutex = texture.cast()?;
        Ok(SharedTexture {
            texture,
            mutex,
            handle: self.handle,
            owns_handle: false,
        })
    }

    pub fn acquire(&self, key: u64, timeout_ms: u32) -> Result<()> {
        unsafe { self.mutex.AcquireSync(key, timeout_ms) }
    }

    pub fn release(&self, key: u64) -> Result<()> {
        unsafe { self.mutex.ReleaseSync(key) }
    }
}

impl Drop for SharedTexture {
    fn drop(&mut self) {
        if self.owns_handle && !self.handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

/// Description string of a DXGI adapter, trimmed at the first NUL.
fn desc_string(desc: &DXGI_ADAPTER_DESC) -> String {
    let len = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    String::from_utf16_lossy(&desc.Description[..len])
}

fn luid_eq(a: &LUID, b: &LUID) -> bool {
    a.LowPart == b.LowPart && a.HighPart == b.HighPart
}

/// The adapter description of the adapter backing a device.
pub fn adapter_desc(device: &ID3D11Device) -> Result<DXGI_ADAPTER_DESC> {
    let a = adapter_of(device)?;
    unsafe { a.GetDesc() }
}

/// Every DXGI adapter in the system with its description.
fn adapters() -> Result<Vec<(IDXGIAdapter, DXGI_ADAPTER_DESC)>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut out = Vec::new();
    let mut i = 0u32;
    while let Ok(a) = unsafe { factory.EnumAdapters(i) } {
        if let Ok(d) = unsafe { a.GetDesc() } {
            out.push((a, d));
        }
        i += 1;
    }
    Ok(out)
}

/// Decide which adapter should host the encoder.
///
/// Returns `Some((adapter, name))` when the encoder should live on a
/// *different*, lower-latency adapter than the one driving the display (which
/// requires moving each frame across adapters with a [`CpuBridge`]), or `None`
/// to keep the encoder on the capture adapter (the fast same-adapter
/// shared-texture path).
///
/// Automatic rule: a discrete NVIDIA display adapter has a high-latency Media
/// Foundation NVENC pipeline, so if an integrated Intel GPU is also present we
/// encode there (Quick Sync, ~4.5 ms) and bridge the pixels over. `force`
/// (from `IMPERIO_ENCODER_ADAPTER`) overrides this: a substring matches an
/// adapter by description, and `same` pins the encoder to the capture adapter.
pub fn pick_encoder_adapter(
    cap_device: &ID3D11Device,
    force: Option<&str>,
) -> Result<Option<(IDXGIAdapter, String)>> {
    let cap_desc = adapter_desc(cap_device)?;
    let cap_name = desc_string(&cap_desc);
    let all = adapters()?;

    if let Some(f) = force.map(str::trim).filter(|f| !f.is_empty()) {
        if f.eq_ignore_ascii_case("same") {
            return Ok(None);
        }
        let needle = f.to_ascii_lowercase();
        if let Some((a, d)) = all
            .iter()
            .find(|(_, d)| desc_string(d).to_ascii_lowercase().contains(&needle))
        {
            if luid_eq(&d.AdapterLuid, &cap_desc.AdapterLuid) {
                return Ok(None); // forced onto the capture adapter itself
            }
            return Ok(Some((a.clone(), desc_string(d))));
        }
        tracing::warn!(force = f, "IMPERIO_ENCODER_ADAPTER matched no adapter; using automatic selection");
    }

    if cap_desc.VendorId == VENDOR_NVIDIA {
        if let Some((a, d)) = all
            .iter()
            .find(|(_, d)| d.VendorId == VENDOR_INTEL && !luid_eq(&d.AdapterLuid, &cap_desc.AdapterLuid))
        {
            tracing::info!(
                capture = %cap_name,
                encoder = %desc_string(d),
                "discrete display adapter; bridging frames to the integrated encoder"
            );
            return Ok(Some((a.clone(), desc_string(d))));
        }
    }
    Ok(None)
}

/// Moves a BGRA frame between two D3D11 devices on different adapters through
/// a CPU staging copy.
///
/// Keyed-mutex shared textures only work within a single adapter, so when the
/// display is driven by a discrete GPU (whose Media Foundation encoder is
/// high-latency) and we want to encode on the integrated GPU's low-latency
/// encoder, the pixels have to travel through system memory. A 1080p BGRA
/// frame is ~8 MB; the round trip costs a couple of milliseconds, far less
/// than the tens of milliseconds the discrete MF encoder pipeline adds.
pub struct CpuBridge {
    /// Capture device, CPU-readable: the frame is copied here then mapped.
    src_staging: ID3D11Texture2D,
    /// Encoder device, CPU-writable: pixels are written here then copied to
    /// the bindable texture below.
    dst_staging: ID3D11Texture2D,
    /// Encoder device, bindable: the video processor reads this as input.
    enc_bgra: ID3D11Texture2D,
    enc_ctx: ID3D11DeviceContext,
    width: u32,
    height: u32,
}

impl CpuBridge {
    pub fn new(
        cap_device: &ID3D11Device,
        enc: &VideoDevice,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let staging_desc = |cpu: D3D11_CPU_ACCESS_FLAG| D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: cpu.0 as u32,
            MiscFlags: 0,
        };
        let mut src = None;
        unsafe {
            cap_device.CreateTexture2D(&staging_desc(D3D11_CPU_ACCESS_READ), None, Some(&mut src))?
        };
        let mut dst = None;
        unsafe {
            enc.device
                .CreateTexture2D(&staging_desc(D3D11_CPU_ACCESS_WRITE), None, Some(&mut dst))?
        };
        let enc_bgra = enc.create_texture(width, height, DXGI_FORMAT_B8G8R8A8_UNORM)?;
        Ok(Self {
            src_staging: src.expect("src staging"),
            dst_staging: dst.expect("dst staging"),
            enc_bgra,
            enc_ctx: enc.context.clone(),
            width,
            height,
        })
    }

    /// Copy `src` (a BGRA texture on the capture device) into the
    /// encoder-device BGRA texture returned by [`input`](Self::input).
    /// `cap_ctx` must be the capture device's immediate context.
    pub fn upload(&self, cap_ctx: &ID3D11DeviceContext, src: &ID3D11Texture2D) -> Result<()> {
        // SAFETY: all resources below outlive the call; the mapped pointers
        // are read/written only within their map/unmap bracket and only up to
        // the texture's dimensions.
        unsafe {
            cap_ctx.CopyResource(&self.src_staging, src);
            let mut smap = D3D11_MAPPED_SUBRESOURCE::default();
            cap_ctx.Map(&self.src_staging, 0, D3D11_MAP_READ, 0, Some(&mut smap))?;
            let mut dmap = D3D11_MAPPED_SUBRESOURCE::default();
            if let Err(e) = self
                .enc_ctx
                .Map(&self.dst_staging, 0, D3D11_MAP_WRITE, 0, Some(&mut dmap))
            {
                cap_ctx.Unmap(&self.src_staging, 0);
                return Err(e);
            }
            let row = (self.width * 4) as usize;
            for y in 0..self.height as usize {
                let s = (smap.pData as *const u8).add(y * smap.RowPitch as usize);
                let d = (dmap.pData as *mut u8).add(y * dmap.RowPitch as usize);
                std::ptr::copy_nonoverlapping(s, d, row);
            }
            self.enc_ctx.Unmap(&self.dst_staging, 0);
            cap_ctx.Unmap(&self.src_staging, 0);
            self.enc_ctx.CopyResource(&self.enc_bgra, &self.dst_staging);
        }
        Ok(())
    }

    /// The encoder-device BGRA texture the converter should read as input.
    pub fn input(&self) -> &ID3D11Texture2D {
        &self.enc_bgra
    }
}

/// Colour space bitfield for `D3D11_VIDEO_PROCESSOR_COLOR_SPACE`:
/// Usage:1 | RGB_Range:1 | YCbCr_Matrix:1 | YCbCr_xvYCC:1 | Nominal_Range:2.
fn color_space(
    rgb_full_range: bool,
    bt709: bool,
    nominal_16_235: bool,
) -> D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
    let mut bits = 0u32;
    if !rgb_full_range {
        bits |= 1 << 1;
    }
    if bt709 {
        bits |= 1 << 2;
    }
    if nominal_16_235 {
        bits |= 1 << 4;
    }
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: bits }
}

/// GPU colour / size conversion via `ID3D11VideoProcessor`. Used for
/// BGRA -> NV12 before encoding and NV12 -> BGRA (with scaling) when
/// rendering.
pub struct Converter {
    dev: VideoDevice,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
}

impl Converter {
    pub fn new(
        dev: &VideoDevice,
        in_w: u32,
        in_h: u32,
        out_w: u32,
        out_h: u32,
        to_nv12: bool,
    ) -> Result<Self> {
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { dev.video_device.CreateVideoProcessorEnumerator(&desc)? };
        let processor = unsafe { dev.video_device.CreateVideoProcessor(&enumerator, 0)? };
        unsafe {
            if to_nv12 {
                dev.video_context.VideoProcessorSetStreamColorSpace(
                    &processor,
                    0,
                    &color_space(true, true, false),
                );
                dev.video_context
                    .VideoProcessorSetOutputColorSpace(&processor, &color_space(true, true, true));
            } else {
                dev.video_context.VideoProcessorSetStreamColorSpace(
                    &processor,
                    0,
                    &color_space(true, true, true),
                );
                dev.video_context
                    .VideoProcessorSetOutputColorSpace(&processor, &color_space(true, true, false));
            }
            // Letterbox bars when a dest rect is set.
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    RGBA: D3D11_VIDEO_COLOR_RGBA {
                        R: 0.0,
                        G: 0.0,
                        B: 0.0,
                        A: 1.0,
                    },
                },
            };
            dev.video_context
                .VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
        }
        Ok(Self {
            dev: dev.clone(),
            enumerator,
            processor,
        })
    }

    /// Restrict output to `rect` (letterboxing); `None` fills the target.
    pub fn set_dest_rect(&self, rect: Option<RECT>) {
        unsafe {
            match rect {
                Some(r) => self.dev.video_context.VideoProcessorSetStreamDestRect(
                    &self.processor,
                    0,
                    true,
                    Some(&r),
                ),
                None => self.dev.video_context.VideoProcessorSetStreamDestRect(
                    &self.processor,
                    0,
                    false,
                    None,
                ),
            }
        }
    }

    pub fn convert(
        &self,
        input: &ID3D11Texture2D,
        input_subresource: u32,
        output: &ID3D11Texture2D,
    ) -> Result<()> {
        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: input_subresource,
                },
            },
        };
        let mut in_view = None;
        unsafe {
            self.dev.video_device.CreateVideoProcessorInputView(
                input,
                &self.enumerator,
                &in_desc,
                Some(&mut in_view),
            )?
        };
        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut out_view = None;
        unsafe {
            self.dev.video_device.CreateVideoProcessorOutputView(
                output,
                &self.enumerator,
                &out_desc,
                Some(&mut out_view),
            )?
        };
        let out_view = out_view.expect("output view");

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(in_view),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let result = unsafe {
            self.dev.video_context.VideoProcessorBlt(
                &self.processor,
                &out_view,
                0,
                &[stream.clone()],
            )
        };
        // SAFETY: we own the view stored in the stream and nothing else
        // references it after the blit.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result
    }
}

pub const NV12: DXGI_FORMAT = DXGI_FORMAT_NV12;
pub const BGRA8: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;
