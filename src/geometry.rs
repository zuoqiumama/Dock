//! Shared, panic-free popup placement in physical monitor coordinates.
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};

pub struct Display {
    pub id: u32,
    pub monitor: windows::Win32::Graphics::Gdi::HMONITOR,
    pub label: String,
}

pub unsafe fn displays() -> Vec<Display> {
    use windows::Win32::Foundation::{BOOL, LPARAM};
    use windows::Win32::Graphics::Gdi::*;
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let list = &mut *(data.0 as *mut Vec<Display>);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            let end = info
                .szDevice
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(info.szDevice.len());
            let name = String::from_utf16_lossy(&info.szDevice[..end]);
            if let Some(id) = name
                .rsplit("DISPLAY")
                .next()
                .and_then(|s| s.parse::<u32>().ok())
            {
                let area = info.monitorInfo.rcMonitor;
                list.push(Display {
                    id,
                    monitor,
                    label: format!(
                        "显示器 {id} · {}×{}",
                        area.right - area.left,
                        area.bottom - area.top
                    ),
                });
            }
        }
        BOOL(1)
    }
    let mut list: Vec<Display> = Vec::new();
    let _ = EnumDisplayMonitors(
        None,
        None,
        Some(collect),
        LPARAM(&mut list as *mut _ as isize),
    );
    list.sort_by_key(|display| display.id);
    list
}

pub unsafe fn selected_monitor(id: u32) -> windows::Win32::Graphics::Gdi::HMONITOR {
    displays()
        .into_iter()
        .find(|d| id != 0 && d.id == id)
        .map(|d| d.monitor)
        .unwrap_or_else(|| {
            windows::Win32::Graphics::Gdi::MonitorFromWindow(
                windows::Win32::Foundation::HWND::default(),
                windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTOPRIMARY,
            )
        })
}

pub fn fit_axis(origin: i32, extent: i32, start: i32, end: i32, margin: i32) -> i32 {
    let margin = margin.max(0).min((end - start).max(0) / 2);
    let low = start.saturating_add(margin);
    let high = end.saturating_sub(margin).saturating_sub(extent.max(0));
    origin.clamp(low, high.max(low))
}

pub unsafe fn work_area(x: i32, y: i32) -> RECT {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(
        MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST),
        &mut info,
    )
    .as_bool()
    {
        info.rcWork
    } else {
        RECT {
            left: x.saturating_sub(640),
            top: y.saturating_sub(480),
            right: x.saturating_add(640),
            bottom: y.saturating_add(480),
        }
    }
}

pub fn columns(width: f32, padding: f32, cell: f32, gap: f32, max: usize) -> usize {
    (((width - padding * 2.0 + gap) / (cell + gap).max(1.0))
        .floor()
        .max(1.0) as usize)
        .min(max.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_and_negative_monitor_coordinates_are_safe() {
        assert_eq!(fit_axis(100, 900, 0, 768, 12), 12);
        assert_eq!(fit_axis(-10, 300, -1920, 0, 8), -308);
        assert_eq!(fit_axis(0, 20, 0, 0, 8), 0);
    }
    #[test]
    fn narrow_drawers_keep_at_least_one_column() {
        assert_eq!(columns(512.0, 14.0, 92.0, 6.0, 5), 5);
        assert_eq!(columns(280.0, 14.0, 92.0, 6.0, 5), 2);
        assert_eq!(columns(16.0, 14.0, 92.0, 6.0, 5), 1);
    }
}

#[cfg(test)]
mod placement_regression_tests {
    use super::*;
    #[test]
    fn placement_is_bounded_across_negative_and_tiny_work_areas() {
        for start in [-3840, -1080, 0, 1920] {
            for width in [0, 1, 16, 768, 1920] {
                for extent in [1, 32, 768, 4000] {
                    let position = fit_axis(10_000, extent, start, start + width, 8);
                    assert!(position >= start);
                    if extent + 16 <= width {
                        assert!(position + extent <= start + width - 8);
                    }
                }
            }
        }
    }
}
