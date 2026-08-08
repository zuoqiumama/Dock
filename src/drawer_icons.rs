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
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{CloseHandle, BOOL, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};

use crate::icons;

const FLAG: &str = "--drawer-icons";
/// Hard cap on one extraction batch. A slow network `.lnk` target can take a while,
/// but the dock must never wait forever for its icons.
const BATCH_TIMEOUT_MS: u32 = 45_000;
/// Console-less spawn for the helper (mirrors the watchdog's creation flags).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Parse `--drawer-icons <job> <outdir>` from the command line. `None` for a
/// normal dock launch �?checked at the very top of `main`, before any GUI setup.
pub fn parse_args() -> Option<(PathBuf, PathBuf)> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some(FLAG) {
        return None;
    }
    Some((PathBuf::from(args.next()?), PathBuf::from(args.next()?)))
}

/// Helper-process entry point: read the job file, extract one icon per line into
/// `<outdir>/<idx>.bmp`, then exit. Any third-party shell crash ends THIS process
/// only. Successes are detected by the dock from the written files, so a partial
/// batch (crash or timeout mid-way) still yields usable icons.
pub fn run(job: &Path, outdir: &Path) {
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let _ = fs::create_dir_all(outdir);
    let Ok(wic) =
        (unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) })
    else {
        return;
    };
    for (idx, path) in read_job(job) {
        extract_bmp(&wic, &path, outdir, idx);
    }
}

fn read_job(job: &Path) -> Vec<(usize, String)> {
    let Ok(text) = fs::read_to_string(job) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '\t');
            let idx = parts.next()?.parse().ok()?;
            let path = unescape(parts.next()?);
            if path.is_empty() {
                None
            } else {
                Some((idx, path))
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
/// silently �?the dock falls back to a glyph tile for anything without a file.
fn extract_bmp(wic: &IWICImagingFactory, path: &str, outdir: &Path, idx: usize) {
    let Some(icon) = (unsafe { icons::source_icon(path, 256) }) else {
        return;
    };
    let Ok(bitmap) = (unsafe { wic.CreateBitmapFromHICON(icon.raw()) }) else {
        return;
    };
    let mut width = 0u32;
    let mut height = 0u32;
    if unsafe { bitmap.GetSize(&mut width, &mut height) }.is_err() {
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
    if unsafe { bitmap.CopyPixels(&rect, stride, &mut pixels) }.is_err() {
        return;
    }
    let target = outdir.join(format!("{idx}.bmp"));
    write_bmp(&target, width, height, &pixels);
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

/// Turn an extracted BMP into a D2D bitmap for the drawer to draw. The pixels are
/// premultiplied (WIC's `CreateBitmapFromHICON` output), matching the alpha mode
/// D2D expects. Returns None for anything unreadable �?the tile falls back to its
/// glyph without touching the shell.
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

/// Dock side: run one extraction batch for `jobs` (`(entry index, path)` pairs) in a
/// helper process. Returns the BMP file written for each successful job index.
/// NEVER panics and NEVER blocks longer than `BATCH_TIMEOUT_MS` �?a crashing or hung
/// shell extension costs icons, not the dock.
pub fn extract(jobs: &[(usize, String)]) -> Vec<(usize, PathBuf)> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let Some(dir) = batch_dir() else {
        return Vec::new();
    };
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::create_dir_all(&dir);
    let job = dir.join("job.tsv");
    let out = dir.join("out");
    let _ = fs::create_dir_all(&out);

    let mut body = String::new();
    for (idx, path) in jobs {
        body.push_str(&format!("{idx}\t{}\n", escape(path)));
    }
    if fs::write(&job, body).is_err() {
        return Vec::new();
    }

    let Ok(exe) = std::env::current_exe() else {
        return Vec::new();
    };
    let spawned = std::process::Command::new(exe)
        .arg(FLAG)
        .arg(&job)
        .arg(&out)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();

    let Some(mut child) = spawned.ok() else {
        return Vec::new();
    };
    {
        // Wait on the process handle with a hard cap; if the helper outlives it
        // (a hung shell call), stop it and take whatever icons were written first.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, BOOL(0), child.id()) };
        match handle {
            Ok(h) if !h.is_invalid() => {
                let result = unsafe { WaitForSingleObject(h, BATCH_TIMEOUT_MS) };
                if result != WAIT_OBJECT_0 {
                    let _ = child.kill();
                }
                let _ = child.wait();
                let _ = unsafe { CloseHandle(h) };
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    let mut results: Vec<(usize, PathBuf)> = Vec::new();
    if let Ok(read) = fs::read_dir(&out) {
        for entry in read.flatten() {
            let file = entry.file_name();
            let Some(name) = file.to_str() else {
                continue;
            };
            let Some(idx) = name.strip_suffix(".bmp").and_then(|stem| stem.parse().ok()) else {
                continue;
            };
            if entry.metadata().map(|m| m.len() > 0).unwrap_or(false) {
                results.push((idx, entry.path()));
            }
        }
    }
    results
}

/// The per-dock temp directory for the current extraction batch. Keyed by dock PID
/// so a crashed dock's leftovers never collide with a later instance's files.
fn batch_dir() -> Option<PathBuf> {
    let mut dir = std::env::temp_dir();
    dir.push(format!("featherdock-drawer-icons-{}", std::process::id()));
    Some(dir)
}

/// Remove the extraction batch directory. Called after the BMPs have been decoded
/// into GPU bitmaps so the temp files don't linger.
pub fn cleanup() {
    if let Some(dir) = batch_dir() {
        let _ = fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let job = dir.join("job.tsv");
        let path = r"C:\Users\Me\Desktop\My App.lnk";
        fs::write(&job, format!("0\t{}\n", escape(path))).unwrap();
        let jobs = read_job(&job);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0], (0, path.to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_or_malformed_job_lines_are_skipped() {
        let dir = std::env::temp_dir().join(format!("fd-icon-mal-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let job = dir.join("job.tsv");
        fs::write(&job, "x\tbad\n1\t\n\n2\tC:\\ok.exe\n").unwrap();
        let jobs = read_job(&job);
        assert_eq!(jobs, vec![(2, r"C:\ok.exe".to_string())]);
        let _ = fs::remove_dir_all(&dir);
    }
}
