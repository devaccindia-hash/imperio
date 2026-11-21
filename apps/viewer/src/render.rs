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
    /// The part of the source picture shown (all of it unless zoomed in).
    src: RECT,
    cropped: bool,
    zoom: Option<f64>,
    pan: (f64, f64),
    /// The picture last drawn, to draw again after a zoom or pan change.
    last: Option<(ID3D11Texture2D, u32)>,
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
            src: RECT::default(),
            cropped: false,
            zoom: None,
            pan: (0.5, 0.5),
            last: None,
        };
        r.rebuild();
        Ok(r)
    }

    pub fn device(&self) -> &VideoDevice {
        &self.dev
    }

    pub fn set_source_size(&mut self, w: u32, h: u32) {
        self.source = Some((w, h));
        self.rebuild();
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
        self.rebuild();
        self.redraw();
        Ok(())
    }

    /// Zoom: `None` fits the whole picture in the window; `Some(f)` shows
    /// `f` window pixels per pixel of the other screen (1.0 = actual size).
    pub fn set_zoom(&mut self, zoom: Option<f64>) {
        self.zoom = zoom;
        self.layout();
        self.redraw();
    }

    pub fn zoom(&self) -> Option<f64> {
        self.zoom
    }

    /// Window pixels per pixel of the other screen, as shown right now.
    pub fn scale(&self) -> f64 {
        let Some((sw, sh)) = self.source else { return 1.0 };
        self.zoom.unwrap_or_else(|| (self.width as f64 / sw as f64).min(self.height as f64 / sh as f64))
    }

    /// When the picture is larger than the window, the part shown follows
    /// the pointer: `fx`, `fy` are its position across the window, 0..=1.
    pub fn set_pan(&mut self, fx: f64, fy: f64) {
        let pan = (fx.clamp(0.0, 1.0), fy.clamp(0.0, 1.0));
        if self.cropped && pan != self.pan {
            self.pan = pan;
            self.layout();
            self.redraw();
        } else {
            self.pan = pan;
        }
    }

    /// A window position as a position on the other screen, 0..=1 each way.
    pub fn to_source(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (sw, sh) = self.source?;
        let (d, s) = (self.dest, self.src);
        let fx = (x - d.left as f64) / (d.right - d.left).max(1) as f64;
        let fy = (y - d.top as f64) / (d.bottom - d.top).max(1) as f64;
        let px = s.left as f64 + fx.clamp(0.0, 1.0) * (s.right - s.left) as f64;
        let py = s.top as f64 + fy.clamp(0.0, 1.0) * (s.bottom - s.top) as f64;
        Some(((px / sw as f64).clamp(0.0, 1.0), (py / sh as f64).clamp(0.0, 1.0)))
    }

    /// New converter for the current sizes, then the layout.
    fn rebuild(&mut self) {
        let Some((sw, sh)) = self.source else { return };
        match Converter::new(&self.dev, sw, sh, self.width, self.height, false) {
            Ok(c) => self.converter = Some(c),
            Err(e) => tracing::error!(error = %e, "video processor init failed"),
        }
        self.layout();
    }

    /// Which part of the picture goes where in the window.
    fn layout(&mut self) {
        let Some((sw, sh)) = self.source else { return };
        let (ww, wh) = (self.width as f64, self.height as f64);
        let scale = self.scale();
        // One axis at a time: what fits is centred, what does not is cropped
        // to the window and positioned by the pan.
        let axis = |source: f64, window: f64, pan: f64| -> (f64, f64, f64, f64) {
            let shown = source * scale;
            if shown <= window + 0.5 {
                let start = ((window - shown) / 2.0).round();
                (0.0, source, start, start + shown.round())
            } else {
                let visible = window / scale;
                let from = ((source - visible) * pan).round();
                (from, from + visible, 0.0, window)
            }
        };
        let (sx0, sx1, dx0, dx1) = axis(sw as f64, ww, self.pan.0);
        let (sy0, sy1, dy0, dy1) = axis(sh as f64, wh, self.pan.1);
        self.src = RECT { left: sx0 as i32, top: sy0 as i32, right: (sx1.round() as i32).min(sw as i32), bottom: (sy1.round() as i32).min(sh as i32) };
        self.dest = RECT { left: dx0 as i32, top: dy0 as i32, right: dx1 as i32, bottom: dy1 as i32 };
        self.cropped = self.src.right - self.src.left < sw as i32 || self.src.bottom - self.src.top < sh as i32;
        if let Some(c) = &self.converter {
            c.set_source_rect(self.cropped.then_some(self.src));
            c.set_dest_rect(Some(self.dest));
        }
    }

    /// Draw the last picture again (after a zoom or pan change; the other
    /// screen may not change for a while).
    fn redraw(&mut self) {
        if let Some((tex, sub)) = self.last.clone() {
            if let Err(e) = self.present(&tex, sub) {
                tracing::debug!(error = %e, "redraw failed");
            }
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
        self.last = Some((nv12.clone(), subresource));
        Ok(())
    }
}
