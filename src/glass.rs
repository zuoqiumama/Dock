//! A standalone translucent GPU surface for popup panels — the same
//! D3D11 + DXGI-composition + DirectComposition + Direct2D stack the dock uses, so
//! a panel composites as a frosted, click-through-free glass layer at near-zero idle
//! cost. Kept separate from `graphics::Gpu` (which owns the dock's icon pipeline) so
//! the panel can be built and torn down on demand without touching the dock.

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

/// Owns the composition surface for one popup window. Draw between `begin()` and
/// `present()`; both operate in device pixels (1 unit = 1px).
pub struct Glass {
    _d3d: ID3D11Device,
    dc: ID2D1DeviceContext,
    swapchain: IDXGISwapChain1,
    _dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
}

impl Glass {
    pub unsafe fn new(hwnd: HWND, width: u32, height: u32) -> Result<Glass> {
        let d3d = create_device(D3D_DRIVER_TYPE_HARDWARE)
            .or_else(|_| create_device(D3D_DRIVER_TYPE_WARP))?;
        let dxdevice: IDXGIDevice = d3d.cast()?;
        if let Ok(dxdevice1) = d3d.cast::<IDXGIDevice1>() {
            let _ = dxdevice1.SetMaximumFrameLatency(1);
        }

        let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let d2device = factory.CreateDevice(&dxdevice)?;
        let dc = d2device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
        dc.SetDpi(96.0, 96.0);

        let adapter = dxdevice.GetAdapter()?;
        let dxfactory: IDXGIFactory2 = adapter.GetParent()?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width.max(1),
            Height: height.max(1),
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

        let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxdevice)?;
        let target = dcomp.CreateTargetForHwnd(hwnd, BOOL(1))?;
        let visual = dcomp.CreateVisual()?;
        visual.SetContent(&swapchain)?;
        target.SetRoot(&visual)?;
        dcomp.Commit()?;

        bind_target(&dc, &swapchain)?;

        Ok(Glass {
            _d3d: d3d,
            dc,
            swapchain,
            _dcomp: dcomp,
            _target: target,
            _visual: visual,
        })
    }

    /// The Direct2D context to draw into. Call `BeginDraw` on it, draw, then
    /// `present()`.
    pub fn dc(&self) -> &ID2D1DeviceContext {
        &self.dc
    }

    #[allow(dead_code)] // reusable surface API; the fixed-size panel doesn't resize
    pub unsafe fn resize(&self, width: u32, height: u32) -> Result<()> {
        self.dc.SetTarget(None::<&ID2D1Image>);
        self.swapchain.ResizeBuffers(
            0,
            width.max(1),
            height.max(1),
            DXGI_FORMAT_UNKNOWN,
            DXGI_SWAP_CHAIN_FLAG(0),
        )?;
        bind_target(&self.dc, &self.swapchain)
    }

    /// Finish the frame: EndDraw + Present (vsync-paced).
    pub unsafe fn present(&self) -> Result<()> {
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
