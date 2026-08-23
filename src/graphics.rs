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

use std::path::{Path, PathBuf};

use crate::content::{classify_path, ContentKind};
use crate::dock::{Dock, DockItem, ItemRole, BASE_ICON};
use crate::drawer_icons;
use crate::icons::IconLoader;
use crate::render;
use crate::windows_list;

pub struct Gpu {
    _d3d: ID3D11Device,
    d2device: ID2D1Device,
    dc: ID2D1DeviceContext,
    swapchain: IDXGISwapChain1,
    _dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    brush: ID2D1SolidColorBrush,
    format: IDWriteTextFormat,
    icons: Vec<Option<ID2D1Bitmap1>>,
    surface_size: (u32, u32),
}

fn surface_resize_needed(current: (u32, u32), requested: (u32, u32)) -> bool {
    current != requested
}

/// The shell-extraction source path for an item's icon (job key == source), or None
/// for items whose icon never comes from the shell (vector glyphs) or that have no
/// source at all. Mirrors the old in-process fallback chain: a pinned item prefers
/// its explicit `icon`, a running window its app exe's path.
unsafe fn icon_source(it: &DockItem) -> Option<String> {
    match it.role {
        ItemRole::Pinned => it.icon.clone().or_else(|| it.path.clone()),
        ItemRole::Running => it.icon.clone().or_else(|| {
            it.hwnd
                .map(|raw| HWND(raw as *mut core::ffi::c_void))
                .and_then(|hwnd| windows_list::process_exe_path(hwnd))
        }),
        _ => None,
    }
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
            d2device,
            dc,
            swapchain,
            _dcomp: dcomp,
            _target: target,
            _visual: visual,
            brush,
            format,
            icons: Vec::new(),
            surface_size: (w, h),
        })
    }

    /// The shared Direct2D device. Popup surfaces (`Glass`) created with this device
    /// can draw bitmaps that were created on it — so icons decoded once at startup
    /// (see `drawer::warm_cache`) are reused by the drawer's own surface instead of
    /// being re-uploaded every time the drawer opens.
    pub fn d2d_device(&self) -> &ID2D1Device {
        &self.d2device
    }

    /// The D3D11 device backing the Direct2D one, needed to create the popup
    /// swapchains that share it.
    pub fn d3d_device(&self) -> &ID3D11Device {
        &self._d3d
    }

    /// The dock's Direct2D device context (bound to the dock swapchain). Only used
    /// as a decode context at startup, before the message loop renders anything.
    pub fn dc(&self) -> &ID2D1DeviceContext {
        &self.dc
    }

    /// Extract + decode the icons for `targets` (indices into `items`).
    ///
    /// The SHELL work (`SHDefExtractIconW`, `SHGetFileInfoW`, the JUMBO image list)
    /// runs in the `--drawer-icons` helper process — those calls activate whatever
    /// third-party shell extensions are registered on the machine, and a buggy one
    /// (observed in the wild: PrivaZer's PrivaMenu6.dll) faults and takes its host
    /// process down; doing this in-process crashed the whole dock whenever a new
    /// window's icon was extracted. On this side only safe work remains: decoding
    /// the helper's BMP files (WIC), decoding pinned images (WIC), and asking a
    /// live window for its own HICON (WM_GETICON — a window message, no shell).
    unsafe fn extract_bitmaps(
        &self,
        items: &[DockItem],
        targets: &[usize],
    ) -> Vec<Option<ID2D1Bitmap1>> {
        let jobs: Vec<(String, String)> = targets
            .iter()
            .filter_map(|&i| items.get(i).and_then(|it| icon_source(it)))
            .map(|src| (src.clone(), src))
            .collect();
        let extracted = drawer_icons::extract(&jobs);
        let loader = IconLoader::new().ok();
        targets
            .iter()
            .map(|&i| {
                let bmp = items
                    .get(i)
                    .and_then(|it| self.decode_icon(loader.as_ref(), it, &extracted));
                #[cfg(debug_assertions)]
                if let Some(it) = items.get(i) {
                    eprintln!("[icon] {:<14} loaded={}", it.label, bmp.is_some());
                }
                bmp
            })
            .collect()
    }

    /// Decode one item's icon from a completed helper batch. Never touches the
    /// shell, so a crashing shell extension can only cost this icon, not the dock.
    unsafe fn decode_icon(
        &self,
        loader: Option<&IconLoader>,
        it: &DockItem,
        extracted: &[(String, PathBuf)],
    ) -> Option<ID2D1Bitmap1> {
        let bmp_from_jobs = |key: &str| -> Option<ID2D1Bitmap1> {
            extracted
                .iter()
                .find(|(job_key, _)| job_key == key)
                .and_then(|(_, bmp)| drawer_icons::load_icon_bitmap(&self.dc, bmp))
        };
        match it.role {
            // Start + Control + Drawer use custom vector glyphs; the divider draws a line.
            ItemRole::Start | ItemRole::Divider | ItemRole::Control | ItemRole::Drawer => None,
            // Running window: its app exe's helper-extracted icon, else the window's
            // own icon (WM_GETICON, owned by the app — don't free).
            ItemRole::Running => {
                if let Some(src) = icon_source(it) {
                    if let Some(bmp) = bmp_from_jobs(&src) {
                        return Some(bmp);
                    }
                }
                it.hwnd
                    .map(|raw| HWND(raw as *mut core::ffi::c_void))
                    .and_then(|hwnd| windows_list::window_icon(hwnd))
                    .and_then(|hicon| loader.and_then(|l| l.load_hicon(&self.dc, hicon)))
            }
            // Pinned item: its own image decodes straight from the file (WIC, no
            // shell); anything else takes the helper-extracted bitmap.
            ItemRole::Pinned => {
                let src = icon_source(it)?;
                let kind = if it.icon.is_some() {
                    classify_path(Path::new(&src))
                } else {
                    it.kind
                };
                if kind == ContentKind::Image {
                    if let Some(bitmap) =
                        loader.and_then(|l| l.load_image(&self.dc, &src, 256).ok())
                    {
                        return Some(bitmap);
                    }
                }
                bmp_from_jobs(&src)
            }
        }
    }

    /// Extract every item's icon into a GPU bitmap (e.g. at startup).
    pub unsafe fn load_icons(&mut self, items: &[DockItem], _dpi: f32) {
        let all: Vec<usize> = (0..items.len()).collect();
        self.icons = self.extract_bitmaps(items, &all);
    }

    /// Realign the icon set to a reconciled item list (see `Dock::reconcile`). For
    /// each merged slot, reuse the existing bitmap (`Some(old_index)`) — including a
    /// closing window's, so it stays drawn while it collapses — or load a fresh one
    /// (`None`). Far cheaper than re-extracting everything on each window open/close.
    pub unsafe fn remap_icons(&mut self, remap: &[Option<usize>], items: &[DockItem]) {
        let mut old = std::mem::take(&mut self.icons);
        let fresh: Vec<usize> = remap
            .iter()
            .enumerate()
            .filter_map(|(p, src)| src.is_none().then_some(p))
            .collect();
        let decoded = self.extract_bitmaps(items, &fresh);
        let mut decoded = decoded.into_iter();
        self.icons = remap
            .iter()
            .map(|src| match src {
                Some(oi) => old.get_mut(*oi).and_then(Option::take),
                None => decoded.next().flatten(),
            })
            .collect();
    }

    /// Drop the icon bitmaps at the given (ascending) indices, to stay in lockstep
    /// with `Dock::take_finished_exits` when collapsed slots are removed.
    pub fn drop_icons(&mut self, removed: &[usize]) {
        if removed.is_empty() {
            return;
        }
        let mut keep = removed.iter().copied().peekable();
        let mut idx = 0;
        self.icons.retain(|_| {
            let drop = keep.peek() == Some(&idx);
            if drop {
                keep.next();
            }
            idx += 1;
            !drop
        });
    }

    pub unsafe fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        if !surface_resize_needed(self.surface_size, (width, height)) {
            return Ok(());
        }
        // Icons are device bitmaps independent of the swapchain backbuffer, so they
        // survive a resize — the reconcile/remap path keeps them aligned to `items`.
        self.dc.SetTarget(None::<&ID2D1Image>);
        self.swapchain.ResizeBuffers(
            0,
            width,
            height,
            DXGI_FORMAT_UNKNOWN,
            DXGI_SWAP_CHAIN_FLAG(0),
        )?;
        bind_target(&self.dc, &self.swapchain)?;
        self.surface_size = (width, height);
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_surface_size_does_not_resize_swapchain() {
        assert!(!surface_resize_needed((1280, 180), (1280, 180)));
        assert!(surface_resize_needed((1280, 180), (1440, 180)));
        assert!(surface_resize_needed((1280, 180), (1280, 200)));
    }

    fn item(role: ItemRole, path: Option<&str>, icon: Option<&str>) -> DockItem {
        DockItem {
            label: String::new(),
            glyph: "",
            color: (0.0, 0.0, 0.0),
            path: path.map(str::to_string),
            icon: icon.map(str::to_string),
            kind: ContentKind::Application,
            role,
            hwnd: None,
            group_key: None,
            windows: Vec::new(),
        }
    }

    #[test]
    fn icon_source_prefers_the_explicit_icon_then_the_path() {
        let pinned = item(ItemRole::Pinned, Some(r"C:\Apps\Tool.exe"), None);
        assert_eq!(
            unsafe { icon_source(&pinned) },
            Some(r"C:\Apps\Tool.exe".to_string())
        );
        let with_icon = item(
            ItemRole::Pinned,
            Some(r"C:\Apps\Tool.exe"),
            Some(r"C:\Apps\Tool.ico"),
        );
        assert_eq!(
            unsafe { icon_source(&with_icon) },
            Some(r"C:\Apps\Tool.ico".to_string())
        );
        let bare = item(ItemRole::Pinned, None, None);
        assert_eq!(unsafe { icon_source(&bare) }, None);
    }

    #[test]
    fn icon_source_skips_vector_and_windowless_items() {
        for role in [
            ItemRole::Start,
            ItemRole::Divider,
            ItemRole::Control,
            ItemRole::Drawer,
        ] {
            assert_eq!(unsafe { icon_source(&item(role, None, None)) }, None);
        }
        // A running item with neither an icon source nor a window has nothing to extract.
        assert_eq!(
            unsafe { icon_source(&item(ItemRole::Running, None, None)) },
            None
        );
    }

    #[test]
    fn icon_source_runs_an_icon_override_for_running_items() {
        let running = item(ItemRole::Running, None, Some(r"C:\Apps\App.exe"));
        assert_eq!(
            unsafe { icon_source(&running) },
            Some(r"C:\Apps\App.exe".to_string())
        );
    }

    /// The dock must NEVER call the shell icon APIs in its own process: they load
    /// third-party shell extensions (observed crashing the dock via PrivaZer's
    /// PrivaMenu6.dll). Shell extraction lives in the `--drawer-icons` helper only.
    #[test]
    fn dock_icon_extraction_never_touches_the_shell_in_process() {
        let source = include_str!("graphics.rs");
        let production = source
            .rsplit_once("#[cfg(test)]")
            .map(|(head, _)| head)
            .expect("graphics.rs production source");

        assert!(
            !production.contains("source_icon"),
            "SHGetFileInfoW-family calls activate third-party shell extensions in-process"
        );
        assert!(
            !production.contains("loader.load("),
            "the in-process shell loader must not be used; route through drawer_icons::extract"
        );
        assert!(
            production.contains("drawer_icons::extract"),
            "dock icons must be extracted by the helper process"
        );
    }
}
