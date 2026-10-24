//! Drawer icon extraction, isolated in a HELPER PROCESS.
//!
//! The drawer's icons come from the Windows Shell (`.lnk` resolution,
//! `SHGetFileInfoW`, the JUMBO image list). Those calls can activate third-party
//! shell extensions registered on this machine (PrivaZer, Baidu, WPS, Lenovo,
//! OneDrive, 7-Zip, �? �?and a buggy one (PrivaMenu6.dll in the wild) can
//! **crash the process that invoked it**. Doing the extraction in-process meant a
//! shell-extension fault took the whole dock down the moment the drawer opened.
//!
//! Instead the dock spawns a second copy of itself in `--drawer-icons` mode (the
//! same one-exe pattern as the taskbar watchdog): the helper performs all of the
//! shell icon calls, copies each icon's premultiplied BGRA pixels into a simple
//! 32bpp BMP next to its job file, and exits. If a shell extension crashes, only
//! the helper dies; the dock keeps whatever BMPs were already written and falls
//! back to glyph tiles for the rest.
//!
//! BMP was chosen over PNG on purpose: it needs no WIC encoder/decoder codec
//! (which can be absent or broken on stripped-down systems), only the core WIC
//! pixel-copy APIs that are always present. A generous wait timeout caps a hung
//! shell (one slow `.lnk` target) so the dock never stalls behind a stuck helper.

use std::fs;
use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::WaitForSingleObject;

use crate::icons;

const FLAG: &str = "--drawer-icons";
/// Hard cap on one extraction batch. A slow network `.lnk` target can take a while,
/// but the dock must never wait forever for its icons.
const BATCH_TIMEOUT_MS: u32 = 45_000;
/// Console-less spawn for the helper (mirrors the watchdog's creation flags).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Parse `--drawer-icons <job> <outdir>` from the command line. `None` for a
/// normal dock launch — checked at the very top of `main`, before any GUI setup.
pub fn parse_args() -> Option<(PathBuf, PathBuf)> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some(FLAG) {
        return None;
    }
    Some((PathBuf::from(args.next()?), PathBuf::from(args.next()?)))
}

static JOB_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// A unique job file per batch. Batches can run CONCURRENTLY — the dock's UI thread
/// extracts a newly-appeared window's icon while the drawer's background thread
/// extracts its missing tiles — and a fixed name had them clobbering each other's
/// job list mid-write.
fn next_job_path(root: &Path) -> PathBuf {
    let seq = JOB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    root.join(format!("job-{seq}.tsv"))
}

/// Helper-process entry point: read the job file, extract one icon per line into
/// `<outdir>/<idx>.bmp`, then exit. Any third-party shell crash ends THIS process
/// only. Successes are detected by the dock from the written files, so a partial
/// batch (crash or timeout mid-way) still yields usable icons.
pub fn run(job: &Path, outdir: &Path) {
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if let Err(error) = fs::create_dir_all(outdir) {
        crate::error_log::write("创建图标输出目录失败", &error);
        return;
    }
    let Ok(wic) =
        (unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) })
    else {
        return;
    };
    for (key, path) in read_job(job) {
        extract_bmp(&wic, &key, &path, outdir);
    }
}

fn read_job(job: &Path) -> Vec<(String, String)> {
    let Ok(text) = fs::read_to_string(job) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '\t');
            let key = unescape(parts.next()?);
            let path = unescape(parts.next()?);
            if key.is_empty() || path.is_empty() {
                None
            } else {
                Some((key, path))
            }
        })
        .collect()
}

fn escape(path: &str) -> String {
    path.replace('\\', "\\\\")
}

fn unescape(value: &str) -> String {
    value.replace("\\\\", "\\")
}

/// Extract one path's icon and write it as a 32bpp BMP. Failures are skipped
/// silently — the dock falls back to a glyph tile for anything without a file.
fn extract_bmp(wic: &IWICImagingFactory, key: &str, path: &str, outdir: &Path) {
    let Some(icon) = (unsafe { icons::source_icon(path, 256) }) else {
        return;
    };
    let Ok(bitmap) = (unsafe { wic.CreateBitmapFromHICON(icon.raw()) }) else {
        return;
    };
    // `CreateBitmapFromHICON` yields STRAIGHT-alpha BGRA, but the dock decodes
    // every BMP as premultiplied (and Direct2D draws premultiplied). Icons whose
    // transparent padding is WHITE (e.g. Clash Verge's round logo) then rendered
    // with white spikes at their corners once the dock magnified them — the cubic
    // interpolation happily blends that full-strength white against tiny alphas.
    // Convert here so the bytes on disk match the declared format everywhere.
    let Some(premultiplied) = (unsafe { to_premultiplied(wic, &bitmap) }) else {
        return;
    };
    let mut width = 0u32;
    let mut height = 0u32;
    if unsafe { premultiplied.GetSize(&mut width, &mut height) }.is_err() {
        return;
    }
    if width == 0 || height == 0 {
        return;
    }
    let stride = width * 4;
    let mut pixels = vec![0u8; (stride * height) as usize];
    let rect = WICRect {
        X: 0,
        Y: 0,
        Width: width as i32,
        Height: height as i32,
    };
    if unsafe { premultiplied.CopyPixels(&rect, stride, &mut pixels) }.is_err() {
        return;
    }
    let target = outdir.join(icon_file_name(key));
    if let Some((width, height, pixels)) = crop_excess_padding(width, height, &pixels) {
        write_bmp(&target, width, height, &pixels);
    } else {
        write_bmp(&target, width, height, &pixels);
    }
}

/// Some shell icons are a small image inside a 256px JUMBO canvas. Scaling that
/// canvas scales the empty space too. Trim only excessive transparent padding;
/// preserve normal icon margins, premultiplied colors and the content's aspect.
/// This scan runs once in the extraction helper, never in the render loop.
fn crop_excess_padding(width: u32, height: u32, pixels: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let pixel_count = (width as usize).checked_mul(height as usize)?;
    if width == 0 || height == 0 || pixels.len() < pixel_count.checked_mul(4)? {
        return None;
    }
    let (mut left, mut top, mut right, mut bottom) = (width, height, 0, 0);
    for (index, pixel) in pixels
        .as_chunks::<4>()
        .0
        .iter()
        .take(pixel_count)
        .enumerate()
    {
        // Ignore almost-invisible alpha noise when measuring the actual glyph.
        if pixel[3] < 8 {
            continue;
        }
        let x = (index % width as usize) as u32;
        let y = (index / width as usize) as u32;
        left = left.min(x);
        top = top.min(y);
        right = right.max(x);
        bottom = bottom.max(y);
    }
    if left > right || top > bottom {
        return None;
    }
    let content_w = right - left + 1;
    let content_h = bottom - top + 1;
    if content_w.max(content_h) as f32 >= width.max(height) as f32 * 0.70 {
        return None;
    }
    let padding = ((content_w.max(content_h) as f32 * 0.06).ceil() as u32).max(1);
    let cropped_w = content_w + padding * 2;
    let cropped_h = content_h + padding * 2;
    let mut cropped = vec![0; cropped_w as usize * cropped_h as usize * 4];
    // Copy a little surrounding alpha fringe as well, keeping the new padding
    // symmetric even when the shell placed the glyph in a corner of its canvas.
    let source_left = left.saturating_sub(padding);
    let source_top = top.saturating_sub(padding);
    let source_right = right.saturating_add(padding).min(width - 1);
    let source_bottom = bottom.saturating_add(padding).min(height - 1);
    let row_bytes = (source_right - source_left + 1) as usize * 4;
    let dest_x = padding - (left - source_left);
    for y in source_top..=source_bottom {
        let source = (y as usize * width as usize + source_left as usize) * 4;
        let dest_y = padding + y - top;
        let dest = (dest_y as usize * cropped_w as usize + dest_x as usize) * 4;
        cropped[dest..dest + row_bytes].copy_from_slice(&pixels[source..source + row_bytes]);
    }
    Some((cropped_w, cropped_h, cropped))
}

/// A view of `source` converted to `32bppPBGRA` (premultiplied). This is the
/// exact conversion the dock's old in-process path performed via
/// `IconLoader::to_bitmap`; the helper must keep doing it now that it owns the
/// shell-side extraction.
unsafe fn to_premultiplied(
    wic: &IWICImagingFactory,
    source: &IWICBitmap,
) -> Option<IWICFormatConverter> {
    let converter = wic.CreateFormatConverter().ok()?;
    converter
        .Initialize(
            source,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeMedianCut,
        )
        .ok()?;
    Some(converter)
}

/// A minimal top-down 32bpp `BI_RGB` BMP: 14-byte file header + 40-byte info
/// header + one row of BGRA pixels per line. Read back by `read_bmp`.
fn write_bmp(path: &Path, width: u32, height: u32, pixels: &[u8]) {
    let Ok(mut file) = fs::File::create(path) else {
        return;
    };
    let file_size = 54u32 + (width * 4) * height;
    let mut header = Vec::with_capacity(54);
    header.extend_from_slice(b"BM");
    header.extend_from_slice(&file_size.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    header.extend_from_slice(&54u32.to_le_bytes());
    header.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER size
    header.extend_from_slice(&(width as i32).to_le_bytes());
    header.extend_from_slice(&(-(height as i32)).to_le_bytes()); // negative = top-down
    header.extend_from_slice(&1u16.to_le_bytes()); // planes
    header.extend_from_slice(&32u16.to_le_bytes()); // bpp
    header.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    header.extend_from_slice(&((width * 4) * height).to_le_bytes()); // image size
    header.extend_from_slice(&0i32.to_le_bytes()); // x ppm
    header.extend_from_slice(&0i32.to_le_bytes()); // y ppm
    header.extend_from_slice(&0u32.to_le_bytes()); // palette colors
    header.extend_from_slice(&0u32.to_le_bytes()); // important colors
    if file.write_all(&header).is_ok() {
        let _ = file.write_all(pixels);
    }
}

/// Parse a `write_bmp`-produced BMP back into `(width, height, BGRA pixels)`.
/// Pure Rust �?no WIC codec involved, so it works even where codecs are missing.
fn read_bmp(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let data = fs::read(path).ok()?;
    if data.len() < 54 || &data[0..2] != b"BM" {
        return None;
    }
    let header = &data[14..54];
    let width = i32::from_le_bytes(header[4..8].try_into().ok()?);
    let height = i32::from_le_bytes(header[8..12].try_into().ok()?);
    let planes = u16::from_le_bytes(header[12..14].try_into().ok()?);
    let bpp = u16::from_le_bytes(header[14..16].try_into().ok()?);
    let compression = u32::from_le_bytes(header[16..20].try_into().ok()?);
    if planes != 1 || bpp != 32 || compression != 0 || width <= 0 || height == 0 {
        return None;
    }
    let height = height.unsigned_abs();
    let stride = width as u32 * 4;
    let needed = (stride as usize) * (height as usize);
    if data.len() < 54 + needed {
        return None;
    }
    Some((width as u32, height, data[54..54 + needed].to_vec()))
}

/// Turn an extracted BMP into a D2D bitmap for the drawer to draw. The helper
/// writes every BMP already premultiplied (see `extract_bmp`), matching the
/// `32bppPBGRA` format declared below and the alpha mode D2D expects. Returns
/// None for anything unreadable — the tile falls back to its glyph without
/// touching the shell.
pub unsafe fn load_icon_bitmap(dc: &ID2D1DeviceContext, path: &Path) -> Option<ID2D1Bitmap1> {
    let (width, height, pixels) = read_bmp(path)?;
    let wic: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
    let src = wic
        .CreateBitmapFromMemory(
            width,
            height,
            &GUID_WICPixelFormat32bppPBGRA,
            width * 4,
            &pixels,
        )
        .ok()?;
    dc.CreateBitmapFromWicBitmap(&src, None).ok()
}

/// Dock side: run one extraction batch for `jobs` (`(entry key, path)` pairs) in a
/// helper process. Returns the BMP file written for each successful job key.
/// NEVER panics and NEVER blocks longer than `BATCH_TIMEOUT_MS` — a crashing or hung
/// shell extension costs icons, not the dock.
///
/// BMPs live in a PERSISTENT per-dock cache directory (`icon_cache_root`), named by
/// the entry key, so they survive drawer open/close cycles — decoding only needs the
/// file, and re-opening never re-extracts an icon that's already on disk.
pub fn extract(jobs: &[(String, String)]) -> Vec<(String, PathBuf)> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let Some(root) = icon_cache_root() else {
        return Vec::new();
    };
    let job = next_job_path(&root);

    let mut body = String::new();
    for (key, path) in jobs {
        body.push_str(&format!("{}\t{}\n", escape(key), escape(path)));
    }
    if let Err(error) = fs::write(&job, body) {
        crate::error_log::write("写入图标提取任务失败", &error);
        return Vec::new();
    }

    let Ok(exe) = std::env::current_exe() else {
        return Vec::new();
    };
    let spawned = std::process::Command::new(exe)
        .arg(FLAG)
        .arg(&job)
        .arg(&root)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();

    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            crate::error_log::write("启动图标提取助手失败", &error);
            return Vec::new();
        }
    };
    {
        // Wait on the process handle with a hard cap; if the helper outlives it
        // (a hung shell call), stop it and take whatever icons were written first.
        // Command already owns a synchronized handle. Reopening the child by
        // PID can be denied in a restricted session and used to kill it early.
        let result =
            unsafe { WaitForSingleObject(HANDLE(child.as_raw_handle()), BATCH_TIMEOUT_MS) };
        if result != WAIT_OBJECT_0 {
            let _ = child.kill();
            crate::error_log::write("图标提取助手等待失败或超时", &format!("wait={result:?}"));
        }
        if let Ok(status) = child.wait() {
            if !status.success() {
                crate::error_log::write("图标提取助手异常退出", &status);
            }
        }
    }

    let mut results: Vec<(String, PathBuf)> = Vec::new();
    for (key, _) in jobs {
        let path = root.join(icon_file_name(key));
        if path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            results.push((key.clone(), path));
        }
    }
    results
}

/// A filesystem-safe file name for one entry key: a short SHA-256 hex digest is
/// enough to keep it unique, stable and short (the helper parses no key out of the
/// file name, so there's no need to keep the key readable).
fn icon_file_name(key: &str) -> String {
    let digest = crate::atomic::hash_hex(key);
    format!("{}.bmp", &digest[..24])
}

/// The persistent per-dock icon cache directory. Keyed by dock PID so a crashed
/// dock's leftovers never collide with a later instance's files.
static ICON_CACHE_ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();

fn icon_cache_root() -> Option<PathBuf> {
    ICON_CACHE_ROOT
        .get_or_init(|| {
            let name = format!("featherdock-drawer-icon-cache-{}", std::process::id());
            let mut candidates = vec![std::env::temp_dir().join(&name)];
            if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty())
            {
                candidates.push(PathBuf::from(local).join("FeatherDock").join(&name));
            }
            if let Ok(exe) = std::env::current_exe() {
                if let Some(parent) = exe.parent() {
                    candidates.push(parent.join("outputs").join(&name));
                }
            }
            match prepare_cache_root(&candidates) {
                Ok(root) => {
                    if candidates.first() != Some(&root) {
                        crate::error_log::write("图标缓存已切换到可写的备用目录", &root.display());
                    }
                    Some(root)
                }
                Err(error) => {
                    crate::error_log::write("无法创建可写的图标缓存目录", &error);
                    None
                }
            }
        })
        .clone()
}

/// A TEMP directory can exist but reject this process's writes. Verify both
/// directory creation and file creation before committing the session to it.
fn prepare_cache_root(candidates: &[PathBuf]) -> std::io::Result<PathBuf> {
    let mut last_error =
        std::io::Error::new(std::io::ErrorKind::NotFound, "no icon cache location");
    for root in candidates {
        let probe = root.join(".write-probe");
        let result = fs::create_dir_all(root)
            .and_then(|()| fs::write(&probe, []))
            .and_then(|()| fs::remove_file(&probe));
        match result {
            Ok(()) => return Ok(root.clone()),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

/// Remove the per-dock icon cache directory. Called at dock exit so the temp files
/// don't linger; the drawer keeps its BMPs across open/close cycles, so this is NOT
/// called after each open.
pub fn cleanup() {
    if let Some(Some(dir)) = ICON_CACHE_ROOT.get() {
        let _ = fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_primary_cache_uses_a_writable_fallback() {
        let root = std::env::temp_dir().join(format!("fd-icon-cache-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let blocked = root.join("not-a-directory");
        fs::write(&blocked, b"occupied").unwrap();
        let fallback = root.join("fallback");
        let selected = prepare_cache_root(&[blocked.join("cache"), fallback.clone()]).unwrap();
        assert_eq!(selected, fallback);
        assert!(!selected.join(".write-probe").exists());
        assert!(prepare_cache_root(&[blocked.join("cache")]).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    fn padded_pixels(size: u32, left: u32, top: u32, extent: u32) -> Vec<u8> {
        let mut pixels = vec![0; (size * size * 4) as usize];
        for y in top..top + extent {
            for x in left..left + extent {
                let offset = ((y * size + x) * 4) as usize;
                pixels[offset..offset + 4].copy_from_slice(&[20, 40, 60, 128]);
            }
        }
        pixels
    }

    #[test]
    fn jumbo_canvas_with_a_small_glyph_is_normalized() {
        let pixels = padded_pixels(256, 0, 0, 16);
        let (width, height, cropped) =
            crop_excess_padding(256, 256, &pixels).expect("trim jumbo padding");
        assert_eq!((width, height), (18, 18));
        assert_eq!(&cropped[0..4], &[0, 0, 0, 0]);
        assert_eq!(
            &cropped[(width as usize + 1) * 4..(width as usize + 2) * 4],
            &[20, 40, 60, 128]
        );
        assert_eq!(
            cropped
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[3] == 128)
                .count(),
            16 * 16
        );
    }

    #[test]
    fn normal_margins_and_transparent_images_are_preserved() {
        assert!(crop_excess_padding(64, 64, &padded_pixels(64, 8, 8, 48)).is_none());
        assert!(crop_excess_padding(32, 32, &[0; 32 * 32 * 4]).is_none());
        assert!(crop_excess_padding(0, 0, &[]).is_none());
        assert!(crop_excess_padding(32, 32, &[0; 4]).is_none());
    }

    #[test]
    fn bmp_round_trips_through_write_and_read() {
        let dir = std::env::temp_dir().join(format!("fd-icon-bmp-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("t.bmp");
        let mut pixels = Vec::with_capacity(8 * 8 * 4);
        for _ in 0..(8 * 8) {
            pixels.extend_from_slice(&[10, 20, 30, 255]);
        }
        write_bmp(&path, 8, 8, &pixels);
        let (w, h, back) = read_bmp(&path).expect("round trip");
        assert_eq!((w, h), (8, 8));
        assert_eq!(back, pixels);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_bmp_rejects_garbage() {
        let dir = std::env::temp_dir().join(format!("fd-icon-bad-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("bad.bmp");
        fs::write(&path, b"not a bitmap at all").unwrap();
        assert!(read_bmp(&path).is_none());
        let truncated = dir.join("short.bmp");
        fs::write(&truncated, b"BM").unwrap();
        assert!(read_bmp(&truncated).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn job_paths_round_trip_through_escaping() {
        let dir = std::env::temp_dir().join(format!("fd-icon-job-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let job = next_job_path(&dir);
        let path = r"C:\Users\Me\Desktop\My App.lnk";
        fs::write(&job, format!("{}\t{}\n", escape(path), escape(path))).unwrap();
        let jobs = read_job(&job);
        assert_eq!(jobs, vec![(path.to_string(), path.to_string())]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_extraction_batches_get_distinct_job_files() {
        // The dock UI thread and the drawer's background thread can both run a
        // batch; sharing one file name made them overwrite each other's job list.
        let root = Path::new("C:\\temp");
        assert_ne!(next_job_path(root), next_job_path(root));
    }

    /// Straight-alpha white padding must become BLACK once premultiplied on the
    /// helper side: misdeclared-as-premultiplied white (255,255,255,0) rendered
    /// as white corner spikes on round icons (Clash Verge) under magnification.
    #[test]
    fn written_icons_premultiply_white_transparent_padding() {
        // 2x2: opaque red, half-transparent white, transparent white, opaque blue.
        let straight: [u8; 16] = [
            0, 0, 255, 255, // (255,0,0,255) BGRA
            255, 255, 255, 128, // (255,255,255,128)
            255, 255, 255, 0, // (255,255,255,0)  <- the spike culprit
            255, 0, 0, 255, // (0,0,255,255)
        ];
        let wic: Option<IWICImagingFactory> = (unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
        })
        .ok();
        let Some(wic) = wic else {
            // COM unavailable in this environment; the conversion itself is
            // exercised on every real extraction.
            return;
        };
        let source = unsafe {
            wic.CreateBitmapFromMemory(2, 2, &GUID_WICPixelFormat32bppBGRA, 8, &straight)
        }
        .expect("straight-alpha source bitmap");
        let converted = unsafe { to_premultiplied(&wic, &source) }.expect("converter");
        let mut out = vec![0u8; 16];
        let rect = WICRect {
            X: 0,
            Y: 0,
            Width: 2,
            Height: 2,
        };
        unsafe {
            converted
                .CopyPixels(&rect, 8, &mut out)
                .expect("copy premultiplied pixels");
        }
        // BGRA: white*alpha shrinks to <= alpha, transparent padding goes black.
        let half: [u8; 4] = out[4..8].try_into().unwrap();
        assert!(
            half[0] <= half[3] && half[1] <= half[3] && half[2] <= half[3],
            "half-transparent white must premultiply to rgb <= alpha, got {half:?}"
        );
        let corner: [u8; 4] = out[8..12].try_into().unwrap();
        assert_eq!(
            corner,
            [0, 0, 0, 0],
            "transparent white padding must premultiply to black, got {corner:?}"
        );
    }

    #[test]
    fn empty_or_malformed_job_lines_are_skipped() {
        let dir = std::env::temp_dir().join(format!("fd-icon-mal-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let job = dir.join("job.tsv");
        fs::write(&job, "\tbad\nC:\\ok.exe\t\n\nC:\\ok.exe\tC:\\ok.exe\n").unwrap();
        let jobs = read_job(&job);
        assert_eq!(
            jobs,
            vec![(r"C:\ok.exe".to_string(), r"C:\ok.exe".to_string())]
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
