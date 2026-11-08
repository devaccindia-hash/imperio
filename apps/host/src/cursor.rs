//! Draw the host's mouse cursor into captured frames.
//!
//! Desktop Duplication frames exclude the pointer, so the viewer would see no
//! cursor. The frame is copied to a GDI-compatible texture and the current
//! cursor is drawn with `DrawIconEx`, which handles monochrome, colour and
//! masked cursors. (A separate cursor-shape channel with a viewer overlay is
//! a later refinement.)

use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_RESOURCE_MISC_GDI_COMPATIBLE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGISurface1;
use windows::Win32::Graphics::Gdi::DeleteObject;
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, DI_DEFAULTSIZE, DI_NORMAL,
    HICON, ICONINFO,
};

pub struct CursorPainter {
    texture: ID3D11Texture2D,
    surface: IDXGISurface1,
    context: ID3D11DeviceContext,
}

impl CursorPainter {
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> anyhow::Result<Self> {
        // SAFETY: valid device; returns its immediate context.
        let context: ID3D11DeviceContext = unsafe { device.GetImmediateContext()? };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0 as u32,
        };
        let mut tex = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        let texture: ID3D11Texture2D = tex.expect("gdi texture");
        let surface: IDXGISurface1 = texture.cast()?;
        Ok(Self { texture, surface, context })
    }

    /// Copy `src` into the scratch texture, draw the cursor if it is visible
    /// and inside `monitor`, and return the scratch texture to copy onward.
    pub fn paint(&self, src: &ID3D11Texture2D, monitor: RECT) -> &ID3D11Texture2D {
        unsafe {
            self.context.CopyResource(&self.texture, src);
        }
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: sized struct; failures just mean "draw nothing".
        if unsafe { GetCursorInfo(&mut info) }.is_err()
            || info.flags != CURSOR_SHOWING
            || info.hCursor.is_invalid()
        {
            return &self.texture;
        }
        let x = info.ptScreenPos.x - monitor.left;
        let y = info.ptScreenPos.y - monitor.top;
        if x < -64
            || y < -64
            || x > monitor.right - monitor.left + 64
            || y > monitor.bottom - monitor.top + 64
        {
            return &self.texture;
        }
        let (hx, hy) = hotspot(HICON(info.hCursor.0));
        unsafe {
            if let Ok(hdc) = self.surface.GetDC(false) {
                let _ = DrawIconEx(
                    hdc,
                    x - hx,
                    y - hy,
                    HICON(info.hCursor.0),
                    0,
                    0,
                    0,
                    None,
                    DI_NORMAL | DI_DEFAULTSIZE,
                );
                let _ = self.surface.ReleaseDC(None);
            }
        }
        &self.texture
    }
}

fn hotspot(icon: HICON) -> (i32, i32) {
    let mut ii = ICONINFO::default();
    // SAFETY: valid HICON; the bitmaps it returns must be freed by us.
    unsafe {
        if GetIconInfo(icon, &mut ii).is_err() {
            return (0, 0);
        }
        if !ii.hbmMask.is_invalid() {
            let _ = DeleteObject(ii.hbmMask.into());
        }
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(ii.hbmColor.into());
        }
    }
    (ii.xHotspot as i32, ii.yHotspot as i32)
}
