//! Extract an application's icon and turn it into a premultiplied Direct2D bitmap.
//! Path: exe -> HICON (SHDefExtractIconW) -> WIC bitmap -> 32bpp PBGRA -> D2D bitmap.

use windows::core::*;
use windows::Win32::Foundation::{E_FAIL, GENERIC_READ};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    SHDefExtractIconW, SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON,
    SHGFI_PIDL, SHGFI_SYSICONINDEX, SHIL_JUMBO,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

use crate::content::ContentKind;

pub struct IconLoader {
    wic: IWICImagingFactory,
}

impl IconLoader {
    pub unsafe fn new() -> Result<IconLoader> {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        Ok(IconLoader { wic })
    }

    /// Returns None (instead of erroring) so a missing icon just falls back to a glyph.
    pub unsafe fn load(
        &self,
        dc: &ID2D1DeviceContext,
        path: &str,
        size: u32,
        kind: ContentKind,
    ) -> Option<ID2D1Bitmap1> {
        if kind == ContentKind::Image {
            if let Ok(bitmap) = self.load_image(dc, path, size) {
                return Some(bitmap);
            }
        }

        // Prefer the highest-resolution source so magnification stays crisp:
        // SHDefExtractIconW at the requested (large) size handles exe/lnk/ico best,
        // the JUMBO (256px) system image list covers folders/files, and the 32px
        // shell icon is only a last resort.
        let hicon = extract(path, size)
            .or_else(|| jumbo_icon(path))
            .or_else(|| shell_icon(path))?;
        let bmp = self.to_bitmap(dc, hicon).ok();
        let _ = DestroyIcon(hicon);
        bmp
    }

    /// Turn an existing HICON (e.g. a running window's icon) into a GPU bitmap.
    /// The caller still owns `hicon` — this does NOT destroy it.
    pub unsafe fn load_hicon(&self, dc: &ID2D1DeviceContext, hicon: HICON) -> Option<ID2D1Bitmap1> {
        self.to_bitmap(dc, hicon).ok()
    }

    /// The icon for a shell item identified by an absolute PIDL — covers virtual
    /// desktop items (此电脑 / 回收站 / …) that have no filesystem path. Uses the 256px
    /// JUMBO system image list so it stays crisp under magnification.
    pub unsafe fn load_pidl(
        &self,
        dc: &ID2D1DeviceContext,
        pidl: *const ITEMIDLIST,
    ) -> Option<ID2D1Bitmap1> {
        let hicon = jumbo_icon_pidl(pidl)?;
        let bmp = self.to_bitmap(dc, hicon).ok();
        let _ = DestroyIcon(hicon);
        bmp
    }

    unsafe fn load_image(
        &self,
        dc: &ID2D1DeviceContext,
        path: &str,
        size: u32,
    ) -> Result<ID2D1Bitmap1> {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let decoder = self.wic.CreateDecoderFromFilename(
            PCWSTR(wide.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnLoad,
        )?;
        let frame = decoder.GetFrame(0)?;
        let mut width = 0;
        let mut height = 0;
        frame.GetSize(&mut width, &mut height)?;
        if width == 0 || height == 0 {
            return Err(Error::new(E_FAIL, "image has no pixels"));
        }

        let scale = (size as f64 / width as f64)
            .min(size as f64 / height as f64)
            .min(1.0);
        let target_w = ((width as f64 * scale).round() as u32).max(1);
        let target_h = ((height as f64 * scale).round() as u32).max(1);
        let scaler = self.wic.CreateBitmapScaler()?;
        scaler.Initialize(&frame, target_w, target_h, WICBitmapInterpolationModeFant)?;

        let conv = self.wic.CreateFormatConverter()?;
        conv.Initialize(
            &scaler,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeMedianCut,
        )?;
        dc.CreateBitmapFromWicBitmap(&conv, None)
    }

    unsafe fn to_bitmap(&self, dc: &ID2D1DeviceContext, hicon: HICON) -> Result<ID2D1Bitmap1> {
        let src = self.wic.CreateBitmapFromHICON(hicon)?;
        let conv = self.wic.CreateFormatConverter()?;
        conv.Initialize(
            &src,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeMedianCut,
        )?;
        Ok(dc.CreateBitmapFromWicBitmap(&conv, None)?)
    }
}

/// The 256px JUMBO system icon for a path — sharp at any dock size. The HICON is
/// freshly created by the image list, so the caller owns it (DestroyIcon).
unsafe fn jumbo_icon(path: &str) -> Option<HICON> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut info = SHFILEINFOW::default();
    let ok = SHGetFileInfoW(
        PCWSTR(wide.as_ptr()),
        FILE_ATTRIBUTE_NORMAL,
        Some(&mut info),
        std::mem::size_of::<SHFILEINFOW>() as u32,
        SHGFI_SYSICONINDEX,
    );
    if ok == 0 {
        return None;
    }
    let list: IImageList = SHGetImageList(SHIL_JUMBO as i32).ok()?;
    list.GetIcon(info.iIcon, ILD_TRANSPARENT.0).ok()
}

/// The 256px JUMBO system icon for an absolute PIDL (virtual shell items). The HICON
/// is freshly created by the image list, so the caller owns it (DestroyIcon).
unsafe fn jumbo_icon_pidl(pidl: *const ITEMIDLIST) -> Option<HICON> {
    let mut info = SHFILEINFOW::default();
    let ok = SHGetFileInfoW(
        PCWSTR(pidl as *const u16),
        FILE_ATTRIBUTE_NORMAL,
        Some(&mut info),
        std::mem::size_of::<SHFILEINFOW>() as u32,
        SHGFI_PIDL | SHGFI_SYSICONINDEX,
    );
    if ok == 0 {
        return None;
    }
    let list: IImageList = SHGetImageList(SHIL_JUMBO as i32).ok()?;
    list.GetIcon(info.iIcon, ILD_TRANSPARENT.0).ok()
}

unsafe fn shell_icon(path: &str) -> Option<HICON> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut info = SHFILEINFOW::default();
    let result = SHGetFileInfoW(
        PCWSTR(wide.as_ptr()),
        FILE_ATTRIBUTE_NORMAL,
        Some(&mut info),
        std::mem::size_of::<SHFILEINFOW>() as u32,
        SHGFI_ICON | SHGFI_LARGEICON,
    );
    if result == 0 || info.hIcon.is_invalid() {
        None
    } else {
        Some(info.hIcon)
    }
}

unsafe fn extract(path: &str, size: u32) -> Option<HICON> {
    let wpath: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut large = HICON::default();
    let _ = SHDefExtractIconW(PCWSTR(wpath.as_ptr()), 0, 0, Some(&mut large), None, size);
    if large.is_invalid() {
        None
    } else {
        Some(large)
    }
}
