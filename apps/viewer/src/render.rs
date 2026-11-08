//! D3D11 flip-model presentation of decoded NV12 frames.

use anyhow::Context;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use imperio_codec::d3d::{Converter, VideoDevice};
use windows::core::Interface;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use winit::window::Window;

pub struct Renderer {
    dev: VideoDevice,
    swapchain: IDXGISwapChain1,
    width: u32,
    height: u32,
    source: Option<(u32, u32)>,
    converter: Option<Converter>,
    dest: RECT,
}

impl Renderer {
    pub fn new(window: &Window) -> anyhow::Result<Self> {
        let hwnd = match window.window_handle()?.as_raw() {
            RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut _),
            _ => anyhow::bail!("not a Win32 window"),
        };
        let size = window.inner_size();
        let dev = VideoDevice::new()?;
        let dxgi_device: IDXGIDevice = dev.device.cast()?;
        let adapter = unsafe { dxgi_device.GetAdapter()? };
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent()? };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.width.max(1),
            Height: size.height.max(1),
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };
        let swapchain =
            unsafe { factory.CreateSwapChainForHwnd(&dev.device, hwnd, &desc, None, None) }
                .context("CreateSwapChainForHwnd")?;
        let mut r = Self {
            dev,
            swapchain,
            width: size.width.max(1),
            height: size.height.max(1),
            source: None,
            converter: None,
            dest: RECT::default(),
        };
        r.recompute();
        Ok(r)
    }

    pub fn device(&self) -> &VideoDevice {
        &self.dev
    }

    pub fn dest_rect(&self) -> RECT {
        self.dest
    }

    pub fn set_source_size(&mut self, w: u32, h: u32) {
        self.source = Some((w, h));
        self.recompute();
    }

    pub fn resize(&mut self, w: u32, h: u32) -> anyhow::Result<()> {
        if w == 0 || h == 0 {
            return Ok(());
        }
        self.width = w;
        self.height = h;
        // No back-buffer references are held between presents, so this is safe.
        unsafe {
            self.swapchain
                .ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))
        }
        .context("ResizeBuffers")?;
        self.recompute();
        Ok(())
    }

    /// Rebuild the converter and letterbox rect for the current sizes.
    fn recompute(&mut self) {
        let Some((sw, sh)) = self.source else { return };
        let scale = (self.width as f64 / sw as f64).min(self.height as f64 / sh as f64);
        let dw = (sw as f64 * scale).round() as i32;
        let dh = (sh as f64 * scale).round() as i32;
        let left = (self.width as i32 - dw) / 2;
        let top = (self.height as i32 - dh) / 2;
        self.dest = RECT {
            left,
            top,
            right: left + dw,
            bottom: top + dh,
        };
        match Converter::new(&self.dev, sw, sh, self.width, self.height, false) {
            Ok(c) => {
                c.set_dest_rect(Some(self.dest));
                self.converter = Some(c);
            }
            Err(e) => tracing::error!(error = %e, "video processor init failed"),
        }
    }

    pub fn present(&mut self, nv12: &ID3D11Texture2D, subresource: u32) -> anyhow::Result<()> {
        let Some(conv) = &self.converter else {
            return Ok(());
        };
        let back: ID3D11Texture2D = unsafe { self.swapchain.GetBuffer(0) }.context("GetBuffer")?;
        conv.convert(nv12, subresource, &back)
            .context("VideoProcessorBlt")?;
        drop(back);
        unsafe { self.swapchain.Present(0, DXGI_PRESENT(0)) }
            .ok()
            .context("Present")?;
        Ok(())
    }
}
