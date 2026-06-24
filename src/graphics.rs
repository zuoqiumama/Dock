//! GPU pipeline: D3D11 + DXGI composition swapchain + Direct2D + DirectComposition.
//! A transparent, GPU-composited window. Idle = DWM just composites a static layer.

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

use crate::dock::{Dock, DockItem, ItemRole, BASE_ICON};
use crate::icons::IconLoader;
use crate::render;
use crate::windows_list;

pub struct Gpu {
    _d3d: ID3D11Device,
    dc: ID2D1DeviceContext,
    swapchain: IDXGISwapChain1,
    _dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    brush: ID2D1SolidColorBrush,
    format: IDWriteTextFormat,
    icons: Vec<Option<ID2D1Bitmap1>>,
}

impl Gpu {
    pub unsafe fn new(hwnd: HWND, w: u32, h: u32, dpi: f32) -> Result<Gpu> {
        // --- D3D11 device (BGRA for Direct2D) ---
        let d3d = create_device(D3D_DRIVER_TYPE_HARDWARE)
            .or_else(|_| create_device(D3D_DRIVER_TYPE_WARP))?;
        let dxdevice: IDXGIDevice = d3d.cast()?;
        // Lowest input latency: don't let the driver queue extra frames.
        let dxdevice1: IDXGIDevice1 = d3d.cast()?;
        let _ = dxdevice1.SetMaximumFrameLatency(1);

        // --- Direct2D device context ---
        let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let d2device = factory.CreateDevice(&dxdevice)?;
        let dc = d2device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
        dc.SetDpi(96.0, 96.0); // work in device pixels (1 unit = 1 px)

        // --- DXGI composition swapchain (premultiplied alpha = transparency) ---
        let adapter = dxdevice.GetAdapter()?;
        let dxfactory: IDXGIFactory2 = adapter.GetParent()?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: w,
            Height: h,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            ..Default::default()
        };
        let swapchain = dxfactory.CreateSwapChainForComposition(&d3d, &desc, None)?;

        // --- DirectComposition tree: target -> visual -> swapchain ---
        let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxdevice)?;
        let target = dcomp.CreateTargetForHwnd(hwnd, BOOL(1))?;
        let visual = dcomp.CreateVisual()?;
        visual.SetContent(&swapchain)?;
        target.SetRoot(&visual)?;
        dcomp.Commit()?;

        bind_target(&dc, &swapchain)?;

        // --- reusable brush + emoji text format ---
        let brush = dc.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
            None,
        )?;
        let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let glyph_px = BASE_ICON * 0.58 * dpi;
        let format = dwrite.CreateTextFormat(
            w!("Segoe UI Emoji"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            glyph_px,
            w!(""),
        )?;
        format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

        Ok(Gpu {
            _d3d: d3d,
            dc,
            swapchain,
            _dcomp: dcomp,
            _target: target,
            _visual: visual,
            brush,
            format,
            icons: Vec::new(),
        })
    }

    /// Extract each app's real icon into a GPU bitmap (once, at startup).
    pub unsafe fn load_icons(&mut self, items: &[DockItem], _dpi: f32) {
        let Ok(loader) = IconLoader::new() else {
            return;
        };
        // Always extract at the icon's max native resolution (256px) so it stays
        // crisp through magnification, then high-quality-downscale at draw time.
        let size = 256u32;
        self.icons = items
            .iter()
            .map(|it| {
                let bmp = match it.role {
                    // Start uses a custom vector glyph; the divider draws a line.
                    ItemRole::Start | ItemRole::Divider => None,
                    // Running window: prefer its app exe's full-res icon; fall back to
                    // the window's own icon (WM_GETICON, owned by the app — don't free).
                    ItemRole::Running => {
                        let hwnd = it.hwnd.map(|raw| HWND(raw as *mut core::ffi::c_void));
                        hwnd.and_then(|hwnd| windows_list::process_exe_path(hwnd))
                            .and_then(|exe| {
                                loader.load(
                                    &self.dc,
                                    &exe,
                                    size,
                                    crate::content::ContentKind::Application,
                                )
                            })
                            .or_else(|| {
                                hwnd.and_then(|hwnd| windows_list::window_icon(hwnd))
                                    .and_then(|hicon| loader.load_hicon(&self.dc, hicon))
                            })
                    }
                    ItemRole::Pinned => {
                        let src = it.icon.as_deref().or(it.path.as_deref());
                        let kind = if it.icon.is_some() {
                            src.map(std::path::Path::new)
                                .map(crate::content::classify_path)
                                .unwrap_or(it.kind)
                        } else {
                            it.kind
                        };
                        src.and_then(|p| loader.load(&self.dc, p, size, kind))
                    }
                };
                #[cfg(debug_assertions)]
                eprintln!("[icon] {:<14} loaded={}", it.label, bmp.is_some());
                bmp
            })
            .collect();
    }

    pub unsafe fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.icons.clear();
        self.dc.SetTarget(None::<&ID2D1Image>);
        self.swapchain.ResizeBuffers(
            0,
            width,
            height,
            DXGI_FORMAT_UNKNOWN,
            DXGI_SWAP_CHAIN_FLAG(0),
        )?;
        bind_target(&self.dc, &self.swapchain)
    }

    pub unsafe fn render(&self, dock: &Dock) -> Result<()> {
        self.dc.BeginDraw();
        render::draw(&self.dc, &self.brush, &self.format, dock, &self.icons);
        self.dc.EndDraw(None, None)?;
        self.swapchain.Present(1, DXGI_PRESENT(0)).ok()?;
        Ok(())
    }
}

unsafe fn create_device(driver_type: D3D_DRIVER_TYPE) -> Result<ID3D11Device> {
    let mut device = None;
    D3D11CreateDevice(
        None,
        driver_type,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        None,
    )?;
    device.ok_or_else(|| Error::new(E_FAIL, "D3D11 did not return a device"))
}

unsafe fn bind_target(dc: &ID2D1DeviceContext, swapchain: &IDXGISwapChain1) -> Result<()> {
    let backbuffer: IDXGISurface = swapchain.GetBuffer(0)?;
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
        ..Default::default()
    };
    let bitmap = dc.CreateBitmapFromDxgiSurface(&backbuffer, Some(&props))?;
    dc.SetTarget(&bitmap);
    Ok(())
}
