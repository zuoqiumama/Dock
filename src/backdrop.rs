//! A clipped system backdrop below the existing DirectComposition swapchain.
//!
//! Windows 11's documented host-backdrop brush samples the content behind the
//! HWND inside DWM. No screen capture, CPU pixel copies or repaint timer is used.
//! The WinRT visual occupies the non-topmost composition slot; the existing
//! DirectComposition target keeps the topmost slot for sharp icons and text.
//! Unsupported systems and accessibility/transparency policies use the normal
//! Direct2D fill instead. DwmEnableBlurBehindWindow is intentionally not used:
//! Microsoft documents that it no longer produces blur starting with Windows 8.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::core::*;
use windows::Foundation::Numerics::{Vector2, Vector3};
use windows::Foundation::{AsyncStatus, IAsyncAction, IPropertyValue, PropertyValue};
use windows::Graphics::Effects::*;
use windows::System::{DispatcherQueue, DispatcherQueueController};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::CLSID_D2D1GaussianBlur;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::WinRT::Composition::ICompositorDesktopInterop;
use windows::Win32::System::WinRT::Graphics::Direct2D::*;
use windows::Win32::System::WinRT::*;
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::*;

const BLUR_STANDARD_DEVIATION: f32 = 18.0;
static FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);
static UPDATE_FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);

struct Runtime {
    compositor: Compositor,
    _queue: Option<DispatcherQueueController>,
}

thread_local! {
    static RUNTIME: RefCell<Option<Rc<Runtime>>> = const { RefCell::new(None) };
    static BACKDROPS: RefCell<HashMap<isize, Weak<BackdropInner>>> = RefCell::new(HashMap::new());
}

struct BackdropInner {
    runtime: Rc<Runtime>,
    _target: DesktopWindowTarget,
    _root: ContainerVisual,
    sprite: SpriteVisual,
    geometry: CompositionRoundedRectangleGeometry,
    frame: Cell<Option<Frame>>,
    failed: Cell<bool>,
    pending: RefCell<Option<IAsyncAction>>,
    needs_commit: Cell<bool>,
}

impl Drop for BackdropInner {
    fn drop(&mut self) {
        // Only the final Rc owner closes this target. Closing releases its HWND
        // slot synchronously so a theme/device refresh can attach a fresh one.
        // Never reset the HWND-wide host-backdrop flag from an old owner.
        let _ = self._target.Close();
    }
}

/// The lower composition slot is shared when a GPU device is replaced before
/// its old owner is dropped. Dropping one owner never clears another one's root.
pub struct Backdrop(Rc<BackdropInner>);

impl Backdrop {
    /// Create a backdrop hidden at opacity zero. Failure is a supported fallback,
    /// and the first reason is recorded in the application's diagnostic log.
    pub unsafe fn try_new(hwnd: HWND) -> Option<Self> {
        match Self::create(hwnd) {
            Ok(backdrop) => Some(backdrop),
            Err(error) => {
                if !FAILURE_REPORTED.swap(true, Ordering::Relaxed) {
                    crate::error_log::write("系统背景模糊不可用，使用普通材质", &error);
                }
                None
            }
        }
    }

    unsafe fn create(hwnd: HWND) -> Result<Self> {
        if !policy_allows_blur() {
            return Err(Error::new(
                E_NOTIMPL,
                "背景模糊已因高对比度、关闭透明效果、远程会话或系统合成状态而禁用",
            ));
        }
        let key = hwnd.0 as isize;
        if let Some(existing) =
            BACKDROPS.with(|cache| cache.borrow().get(&key).and_then(Weak::upgrade))
        {
            return if existing.failed.get() {
                Err(Error::new(E_FAIL, "背景模糊设备已失效"))
            } else {
                Ok(Self(existing))
            };
        }

        // This documented attribute is supported from Windows 11 build 22000.
        // Its HRESULT is the capability check; no guessed OS version is needed.
        let enabled = BOOL(1);
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_HOSTBACKDROPBRUSH,
            &enabled as *const _ as *const _,
            std::mem::size_of_val(&enabled) as u32,
        )?;
        let runtime = runtime()?;
        let interop: ICompositorDesktopInterop = runtime.compositor.cast()?;
        let target = interop.CreateDesktopWindowTarget(hwnd, BOOL(0))?;
        let root = runtime.compositor.CreateContainerVisual()?;
        let sprite = runtime.compositor.CreateSpriteVisual()?;
        sprite.SetOpacity(0.0)?;

        // Backdrop brushes are inputs to effects, not standalone image brushes.
        // This minimal D2D Gaussian effect avoids bringing in Win2D or XAML.
        let source = CompositionEffectSourceParameter::Create(&HSTRING::from("Backdrop"))?;
        let effect: IGraphicsEffect = BlurEffect {
            source: source.cast()?,
        }
        .into();
        let brush = runtime
            .compositor
            .CreateEffectFactory(&effect)?
            .CreateBrush()?;
        brush.SetSourceParameter(
            &HSTRING::from("Backdrop"),
            &runtime.compositor.CreateHostBackdropBrush()?,
        )?;
        sprite.SetBrush(&brush)?;
        let geometry = runtime.compositor.CreateRoundedRectangleGeometry()?;
        sprite.SetClip(
            &runtime
                .compositor
                .CreateGeometricClipWithGeometry(&geometry)?,
        )?;
        root.Children()?.InsertAtTop(&sprite)?;
        target.SetRoot(&root)?;
        let inner = Rc::new(BackdropInner {
            runtime,
            _target: target,
            _root: root,
            sprite,
            geometry,
            frame: Cell::new(None),
            failed: Cell::new(false),
            pending: RefCell::new(None),
            needs_commit: Cell::new(false),
        });
        BACKDROPS.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.retain(|_, backdrop| backdrop.strong_count() > 0);
            cache.insert(key, Rc::downgrade(&inner));
        });
        Ok(Self(inner))
    }

    /// `bounds` is left/top/right/bottom in the HWND's device pixels. Match the
    /// foreground's transform and opacity so opening panels never flash a full
    /// backdrop before their content appears. Unchanged frames do no COM work.
    pub fn update(&self, bounds: (f32, f32, f32, f32), radius: f32, opacity: f32) -> Result<()> {
        if self.0.failed.get() {
            return Err(Error::new(E_FAIL, "背景模糊设备已失效"));
        }
        let result = (|| {
            // Check the previous asynchronous result even if this frame did not
            // change. No timer or wake message is needed just to poll a commit.
            self.check_commit()?;
            self.apply_frame(bounds, radius, opacity)?;
            if self.0.needs_commit.get() && self.0.pending.borrow().is_none() {
                let action = self.0.runtime.compositor.RequestCommitAsync()?;
                *self.0.pending.borrow_mut() = Some(action);
                self.0.needs_commit.set(false);
            }
            Ok(())
        })();
        if let Err(error) = &result {
            self.0.failed.set(true);
            let _ = self.0.sprite.SetOpacity(0.0);
            // Closing only this lower target also removes an old material if
            // the compositor cannot accept a final opacity update.
            let _ = self.0._target.Close();
            self.0.pending.borrow_mut().take();
            if !UPDATE_FAILURE_REPORTED.swap(true, Ordering::Relaxed) {
                crate::error_log::write("背景模糊更新失败，已切换普通材质", error);
            }
        }
        result
    }

    pub fn is_usable(&self) -> bool {
        !self.0.failed.get()
    }

    fn check_commit(&self) -> Result<()> {
        let mut pending = self.0.pending.borrow_mut();
        if let Some(action) = pending.as_ref() {
            match action.Status()? {
                AsyncStatus::Started => return Ok(()),
                AsyncStatus::Completed | AsyncStatus::Error => action.GetResults()?,
                AsyncStatus::Canceled => {
                    return Err(Error::new(E_ABORT, "背景模糊提交被取消"));
                }
                _ => return Err(Error::new(E_FAIL, "未知的背景模糊提交状态")),
            }
        }
        *pending = None;
        Ok(())
    }

    fn apply_frame(&self, bounds: (f32, f32, f32, f32), radius: f32, opacity: f32) -> Result<()> {
        let next = Frame::new(bounds, radius, opacity);
        let previous = self.0.frame.get();
        if previous == Some(next) {
            return Ok(());
        }
        if previous.map(|frame| (frame.left, frame.top)) != Some((next.left, next.top)) {
            self.0.sprite.SetOffset(Vector3 {
                X: next.left,
                Y: next.top,
                Z: 0.0,
            })?;
        }
        if previous.map(|frame| (frame.width, frame.height)) != Some((next.width, next.height)) {
            let size = Vector2 {
                X: next.width,
                Y: next.height,
            };
            self.0.sprite.SetSize(size)?;
            self.0.geometry.SetSize(size)?;
        }
        if previous.map(|frame| frame.radius) != Some(next.radius) {
            self.0.geometry.SetCornerRadius(Vector2 {
                X: next.radius,
                Y: next.radius,
            })?;
        }
        if previous.map(|frame| frame.opacity) != Some(next.opacity) {
            self.0.sprite.SetOpacity(next.opacity)?;
        }
        // Coalesce changes while one commit is in flight. A later natural
        // update requests the next fence; the compositor also batches property
        // changes automatically. There is at most one outstanding action.
        self.0.needs_commit.set(true);
        self.0.frame.set(Some(next));
        Ok(())
    }
}

/// Call on the UI thread after releasing every Backdrop and destroying the app's
/// windows. A queue created on the current thread needs its message loop to run
/// until shutdown finishes; waiting in TLS Drop would deadlock that loop.
pub unsafe fn shutdown() {
    BACKDROPS.with(|cache| cache.borrow_mut().clear());
    let Some(runtime) = RUNTIME.with(|cached| cached.borrow_mut().take()) else {
        return;
    };
    let queue = runtime._queue.clone();
    // Release the compositor before asking its owned dispatcher queue to stop.
    let _ = runtime.compositor.Close();
    drop(runtime);
    let Some(queue) = queue else {
        return; // A dispatcher queue supplied by another host is never ours to stop.
    };
    let mut quit_code = None;
    let result = (|| -> Result<()> {
        let action = queue.ShutdownQueueAsync()?;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match action.Status()? {
                AsyncStatus::Completed | AsyncStatus::Error => return action.GetResults(),
                AsyncStatus::Canceled => {
                    return Err(Error::new(E_ABORT, "背景调度队列关闭被取消"));
                }
                AsyncStatus::Started => {}
                _ => return Err(Error::new(E_FAIL, "未知的背景调度队列关闭状态")),
            }
            if Instant::now() >= deadline {
                return Err(Error::new(E_ABORT, "背景调度队列关闭超时，已继续退出"));
            }
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                if message.message == WM_QUIT {
                    quit_code = Some(message.wParam.0 as i32);
                } else {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(16) as u32;
            MsgWaitForMultipleObjectsEx(None, wait, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
    })();
    if let Some(code) = quit_code {
        PostQuitMessage(code);
    }
    if let Err(error) = result {
        crate::error_log::write("背景调度队列关闭未完成", &error);
    }
}

unsafe fn runtime() -> Result<Rc<Runtime>> {
    RUNTIME.with(|cached| {
        if let Some(runtime) = cached.borrow().as_ref() {
            return Ok(runtime.clone());
        }
        let queue = if DispatcherQueue::GetForCurrentThread().is_ok() {
            None
        } else {
            Some(CreateDispatcherQueueController(DispatcherQueueOptions {
                dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
                threadType: DQTYPE_THREAD_CURRENT,
                apartmentType: DQTAT_COM_STA,
            })?)
        };
        let runtime = Rc::new(Runtime {
            compositor: Compositor::new()?,
            _queue: queue,
        });
        *cached.borrow_mut() = Some(runtime.clone());
        Ok(runtime)
    })
}

unsafe fn policy_allows_blur() -> bool {
    if GetSystemMetrics(SM_REMOTESESSION) != 0
        || !DwmIsCompositionEnabled()
            .map(|enabled| enabled.as_bool())
            .unwrap_or(false)
    {
        return false;
    }
    let mut contrast = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    if SystemParametersInfoW(
        SPI_GETHIGHCONTRAST,
        contrast.cbSize,
        Some(&mut contrast as *mut _ as *mut _),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_ok()
        && (contrast.dwFlags & HCF_HIGHCONTRASTON).0 != 0
    {
        return false;
    }
    let mut enabled = 1u32;
    let mut bytes = std::mem::size_of_val(&enabled) as u32;
    let read = RegGetValueW(
        HKEY_CURRENT_USER,
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
        w!("EnableTransparency"),
        RRF_RT_REG_DWORD,
        None,
        Some(&mut enabled as *mut _ as *mut _),
        Some(&mut bytes),
    );
    read != ERROR_SUCCESS || enabled != 0
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Frame {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    radius: f32,
    opacity: f32,
}

impl Frame {
    fn new(bounds: (f32, f32, f32, f32), radius: f32, opacity: f32) -> Self {
        let (left, top, right, bottom) = bounds;
        let valid = [left, top, right, bottom, radius, opacity]
            .iter()
            .all(|n| n.is_finite())
            && right > left
            && bottom > top;
        if !valid {
            return Self {
                left: 0.0,
                top: 0.0,
                width: 0.0,
                height: 0.0,
                radius: 0.0,
                opacity: 0.0,
            };
        }
        let width = right - left;
        let height = bottom - top;
        Self {
            left,
            top,
            width,
            height,
            radius: radius.clamp(0.0, width.min(height) * 0.5),
            opacity: opacity.clamp(0.0, 1.0),
        }
    }
}

#[implement(IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectD2D1Interop)]
struct BlurEffect {
    source: IGraphicsEffectSource,
}

impl IGraphicsEffectSource_Impl for BlurEffect_Impl {}

impl IGraphicsEffect_Impl for BlurEffect_Impl {
    fn Name(&self) -> Result<HSTRING> {
        Ok(HSTRING::from("FeatherDockBackdropBlur"))
    }
    fn SetName(&self, _: &HSTRING) -> Result<()> {
        Ok(())
    }
}

impl IGraphicsEffectD2D1Interop_Impl for BlurEffect_Impl {
    fn GetEffectId(&self) -> Result<GUID> {
        Ok(CLSID_D2D1GaussianBlur)
    }
    fn GetNamedPropertyMapping(
        &self,
        _: &PCWSTR,
        _: *mut u32,
        _: *mut GRAPHICS_EFFECT_PROPERTY_MAPPING,
    ) -> Result<()> {
        Err(E_INVALIDARG.into())
    }
    fn GetPropertyCount(&self) -> Result<u32> {
        Ok(3)
    }
    fn GetProperty(&self, index: u32) -> Result<IPropertyValue> {
        match index {
            0 => PropertyValue::CreateSingle(BLUR_STANDARD_DEVIATION)?.cast(),
            1 => PropertyValue::CreateUInt32(1)?.cast(), // balanced optimization
            2 => PropertyValue::CreateUInt32(1)?.cast(), // hard edge: extend edge pixels
            _ => Err(E_INVALIDARG.into()),
        }
    }
    fn GetSource(&self, index: u32) -> Result<IGraphicsEffectSource> {
        if index == 0 {
            Ok(self.source.clone())
        } else {
            Err(E_INVALIDARG.into())
        }
    }
    fn GetSourceCount(&self) -> Result<u32> {
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backdrop_bounds_hide_invalid_frames_and_limit_rounding() {
        assert_eq!(Frame::new((0.0, 0.0, 20.0, 10.0), 100.0, 2.0).radius, 5.0);
        assert_eq!(Frame::new((0.0, 0.0, 20.0, 10.0), 4.0, 2.0).opacity, 1.0);
        assert_eq!(Frame::new((0.0, 0.0, 0.0, 10.0), 4.0, 1.0).opacity, 0.0);
        assert_eq!(
            Frame::new((f32::NAN, 0.0, 20.0, 10.0), 4.0, 1.0).opacity,
            0.0
        );
    }
}
