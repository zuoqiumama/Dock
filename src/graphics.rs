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
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::backdrop::Backdrop;
use crate::content::{classify_path, ContentKind};
use crate::dock::{Dock, DockItem, ItemRole, BASE_ICON};
use crate::drawer_icons;
use crate::icons::IconLoader;
use crate::render;
use crate::theme::ThemePreset;
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
    materials: render::MaterialBrushes,
    icons: Vec<Option<ID2D1Bitmap1>>,
    surface_size: (u32, u32),
    backdrop: Option<Backdrop>,
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

const ICON_DECODE_BATCH: usize = 4;

struct IconExtractionPayload {
    requested: Vec<String>,
    results: Vec<(String, PathBuf)>,
}

/// Lives on the UI thread, independently of any particular D2D device. A worker
/// already running during device recovery therefore keeps ownership of its jobs;
/// a replacement `Gpu` can reuse its BMPs without extracting the same source twice.
#[derive(Default)]
struct IconPipeline {
    owner: isize,
    active: bool,
    pending_extraction: VecDeque<String>,
    queued_sources: BTreeSet<String>,
    pending_decode: VecDeque<(String, PathBuf)>,
}

std::thread_local! {
    static ICON_PIPELINE: RefCell<IconPipeline> = RefCell::new(IconPipeline::default());
}

/// Start at most one helper batch. Only owned job strings and the raw owner address
/// cross the thread boundary; COM, D2D and `Gpu` itself stay on the UI thread.
fn pump_icon_pipeline() {
    let Some((owner, requested, jobs)) = ICON_PIPELINE.with(|pipeline| {
        let mut pipeline = pipeline.borrow_mut();
        if pipeline.active || pipeline.pending_extraction.is_empty() {
            return None;
        }
        let requested: Vec<String> = pipeline.pending_extraction.drain(..).collect();
        let jobs: Vec<(String, String)> = requested
            .iter()
            .map(|source| (source.clone(), source.clone()))
            .collect();
        pipeline.active = true;
        Some((pipeline.owner, requested, jobs))
    }) else {
        return;
    };

    std::thread::spawn(move || {
        // A panic must still complete the batch, or `active` would block every
        // future extraction. The helper itself remains the crash-isolation boundary.
        let results = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drawer_icons::extract(&jobs)
        }))
        .unwrap_or_default();
        let payload = Box::into_raw(Box::new(IconExtractionPayload { requested, results }));
        let posted = unsafe {
            PostMessageW(
                HWND(owner as *mut core::ffi::c_void),
                crate::WM_DOCK_ICONS_READY,
                WPARAM(0),
                LPARAM(payload as isize),
            )
        }
        .is_ok();
        if !posted {
            drop(unsafe { Box::from_raw(payload) });
        }
    });
}

fn enqueue_missing_sources<T>(
    pipeline: &mut IconPipeline,
    items: &[DockItem],
    icons: &[Option<T>],
) {
    for (item, bitmap) in items.iter().zip(icons) {
        if bitmap.is_some() {
            continue;
        }
        let Some(source) = (unsafe { icon_source(item) }) else {
            continue;
        };
        if pipeline.queued_sources.insert(source.clone()) {
            pipeline.pending_extraction.push_back(source);
        }
    }
}

fn take_decode_batch(pending: &mut VecDeque<(String, PathBuf)>) -> Vec<(String, PathBuf)> {
    let count = pending.len().min(ICON_DECODE_BATCH);
    pending.drain(..count).collect()
}

/// Resolve a completed job against the current list, never against indices captured
/// when extraction started. Empty slots with the same source share one decode.
fn decoded_icon_targets<T>(items: &[DockItem], icons: &[Option<T>], source: &str) -> Vec<usize> {
    items
        .iter()
        .zip(icons)
        .enumerate()
        .filter_map(|(index, (item, bitmap))| {
            if bitmap.is_some() {
                return None;
            }
            (unsafe { icon_source(item) }.as_deref() == Some(source)).then_some(index)
        })
        .collect()
}

fn install_decoded_icon<T: Clone>(icons: &mut [Option<T>], targets: &[usize], bitmap: T) {
    for &index in targets {
        if let Some(slot) = icons.get_mut(index) {
            if slot.is_none() {
                *slot = Some(bitmap.clone());
            }
        }
    }
}

impl Gpu {
    pub unsafe fn new(hwnd: HWND, w: u32, h: u32, dpi: f32) -> Result<Gpu> {
        ICON_PIPELINE.with(|pipeline| {
            pipeline.borrow_mut().owner = hwnd.0 as isize;
        });

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
        let materials = render::MaterialBrushes::new(&dc)?;

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
            materials,
            icons: Vec::new(),
            surface_size: (w, h),
            backdrop: Backdrop::try_new(hwnd),
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

    /// The dock's Direct2D device context (bound to the dock swapchain), also used
    /// to decode completed icon batches on the UI thread.
    pub fn dc(&self) -> &ID2D1DeviceContext {
        &self.dc
    }

    /// Decode one item's icon from a completed helper batch. Never touches the
    /// shell, so a crashing shell extension can only cost this icon, not the dock.
    /// The running-window WM_GETICON fallback is still synchronous and may block on
    /// an unresponsive owner; moving that fallback off the UI thread is future work.
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

    /// Start loading every missing icon without waiting for the shell. Bitmap slots
    /// are filled later by `decode_pending_icons`, including after device recovery.
    pub unsafe fn load_icons(&mut self, items: &[DockItem], _dpi: f32) {
        self.icons.clear();
        self.icons.resize_with(items.len(), || None);
        ICON_PIPELINE.with(|pipeline| {
            enqueue_missing_sources(&mut pipeline.borrow_mut(), items, &self.icons);
        });
        pump_icon_pipeline();
    }

    /// Realign the icon set to a reconciled item list (see `Dock::reconcile`). For
    /// each merged slot, reuse the existing bitmap (`Some(old_index)`) — including a
    /// closing window's, so it stays drawn while it collapses. New and still-empty
    /// slots join the shared background extraction queue instead of blocking here.
    pub unsafe fn remap_icons(&mut self, remap: &[Option<usize>], items: &[DockItem]) {
        let mut old = std::mem::take(&mut self.icons);
        self.icons = remap
            .iter()
            .map(|src| match src {
                Some(oi) => old.get_mut(*oi).and_then(Option::take),
                None => None,
            })
            .collect();
        ICON_PIPELINE.with(|pipeline| {
            enqueue_missing_sources(&mut pipeline.borrow_mut(), items, &self.icons);
        });
        pump_icon_pipeline();
    }

    /// Take ownership of a worker's private completion payload. Extraction failures
    /// become retryable on a later remap; successful BMPs stay deduplicated until
    /// their decode is applied or discarded because the item disappeared.
    pub unsafe fn icons_ready(&mut self, _items: &[DockItem], payload: isize) {
        if payload == 0 {
            return;
        }
        let IconExtractionPayload { requested, results } =
            *Box::from_raw(payload as *mut IconExtractionPayload);
        ICON_PIPELINE.with(|pipeline| {
            let mut pipeline = pipeline.borrow_mut();
            pipeline.active = false;
            let succeeded: BTreeSet<&str> =
                results.iter().map(|(source, _)| source.as_str()).collect();
            for source in &requested {
                if !succeeded.contains(source.as_str()) {
                    pipeline.queued_sources.remove(source);
                }
            }
            drop(succeeded);
            pipeline.pending_decode.extend(results);
        });
        pump_icon_pipeline();
    }

    /// Decode at most four completed BMPs on the UI thread. The return value only
    /// reports queued decoding work; a running extraction posts its own completion
    /// message and must never keep the idle frame loop awake.
    pub unsafe fn decode_pending_icons(&mut self, items: &[DockItem]) -> bool {
        let batch = ICON_PIPELINE
            .with(|pipeline| take_decode_batch(&mut pipeline.borrow_mut().pending_decode));
        if batch.is_empty() {
            return false;
        }

        let loader = IconLoader::new().ok();
        for result in batch {
            let targets = decoded_icon_targets(items, &self.icons, &result.0);
            let decoded = targets
                .first()
                .and_then(|&index| items.get(index))
                .and_then(|item| {
                    self.decode_icon(loader.as_ref(), item, std::slice::from_ref(&result))
                });
            #[cfg(debug_assertions)]
            for &index in &targets {
                if let Some(item) = items.get(index) {
                    eprintln!("[icon] {:<14} loaded={}", item.label, decoded.is_some());
                }
            }
            if let Some(bitmap) = decoded {
                install_decoded_icon(&mut self.icons, &targets, bitmap);
            }
            ICON_PIPELINE.with(|pipeline| {
                pipeline.borrow_mut().queued_sources.remove(&result.0);
            });
        }

        ICON_PIPELINE.with(|pipeline| !pipeline.borrow().pending_decode.is_empty())
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
        let frame = dock.frame();
        let backdrop_enabled = if let Some(backdrop) = &self.backdrop {
            backdrop
                .update(
                    frame.pill,
                    dock.theme.visual().pill_radius * dock.dpi,
                    if dock.theme == ThemePreset::Glass {
                        1.0
                    } else {
                        0.0
                    },
                )
                .is_ok()
        } else {
            false
        };
        self.dc.BeginDraw();
        render::draw(
            &self.dc,
            &self.brush,
            &self.format,
            &self.materials,
            backdrop_enabled,
            dock,
            &self.icons,
        );
        self.dc.EndDraw(None, None)?;
        self.swapchain.Present(1, DXGI_PRESENT(0)).ok()?;
        Ok(())
    }

    /// Re-evaluate OS accessibility/transparency policy after a settings broadcast.
    pub unsafe fn refresh_backdrop(&mut self, hwnd: HWND) {
        self.backdrop = None;
        self.backdrop = Backdrop::try_new(hwnd);
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

    #[test]
    fn missing_sources_are_queued_once_and_vector_items_are_skipped() {
        let source = r"C:\Apps\Tool.exe";
        let items = vec![
            item(ItemRole::Pinned, Some(source), None),
            item(ItemRole::Start, None, None),
            item(ItemRole::Pinned, Some(source), None),
        ];
        let icons: Vec<Option<u8>> = vec![None; items.len()];
        let mut pipeline = IconPipeline::default();

        enqueue_missing_sources(&mut pipeline, &items, &icons);
        enqueue_missing_sources(&mut pipeline, &items, &icons);

        assert_eq!(pipeline.pending_extraction.len(), 1);
        assert_eq!(pipeline.pending_extraction[0], source);
        assert_eq!(pipeline.queued_sources.len(), 1);
        assert!(pipeline.queued_sources.contains(source));
        assert!(!pipeline.active);
        assert!(pipeline.pending_decode.is_empty());
    }

    #[test]
    fn completed_icons_match_current_source_not_an_old_index() {
        let source = r"C:\Apps\Tool.exe";
        let items = vec![
            item(ItemRole::Pinned, Some(r"C:\Apps\Other.exe"), None),
            item(ItemRole::Pinned, Some(source), None),
            item(ItemRole::Pinned, Some(source), None),
            item(ItemRole::Pinned, Some(source), None),
        ];
        // The source's old slot was 0. After reconciliation its current empty slots
        // are 1 and 2; slot 3 already has a bitmap and must not be overwritten.
        let mut icons = vec![Some(7), None, None, Some(9)];
        let targets = decoded_icon_targets(&items, &icons, source);
        assert_eq!(targets, [1, 2]);

        install_decoded_icon(&mut icons, &targets, 42);
        assert_eq!(icons, [Some(7), Some(42), Some(42), Some(9)]);
    }

    #[test]
    fn icon_decode_batches_are_limited_to_four_results() {
        let mut pending: VecDeque<(String, PathBuf)> = (0..6)
            .map(|index| (format!("source-{index}"), PathBuf::new()))
            .collect();

        let first = take_decode_batch(&mut pending);
        assert_eq!(first.len(), ICON_DECODE_BATCH);
        assert_eq!(first.len(), 4);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].0, "source-4");

        let second = take_decode_batch(&mut pending);
        assert_eq!(second.len(), 2);
        assert!(pending.is_empty());
    }

    #[test]
    fn load_and_remap_never_call_shell_extraction_synchronously() {
        let source = include_str!("graphics.rs");
        let production = source
            .rsplit_once("#[cfg(test)]")
            .map(|(head, _)| head)
            .expect("graphics.rs production source");
        let entry_points = production
            .split_once("    pub unsafe fn load_icons")
            .and_then(|(_, tail)| tail.split_once("    pub unsafe fn icons_ready"))
            .map(|(body, _)| body)
            .expect("load_icons and remap_icons source");

        assert!(!entry_points.contains("drawer_icons::extract"));
        assert!(!entry_points.contains("extract_bitmaps"));
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
