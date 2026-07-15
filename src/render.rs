//! Direct2D drawing: the translucent pill + magnified icon tiles + emoji glyphs.
//! Only transforms change per frame; geometry is vector so it stays crisp at any scale.

use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;

use crate::dock::{Dock, ItemRole, BASE_ICON, CORNER, ENTER_RISE};

const IDENTITY: Matrix3x2 = Matrix3x2 {
    M11: 1.0,
    M12: 0.0,
    M21: 0.0,
    M22: 1.0,
    M31: 0.0,
    M32: 0.0,
};

#[derive(Clone, Copy)]
struct RunningDotStyle {
    rgb: (f32, f32, f32),
    alpha: f32,
}

fn fit_rect(container: D2D_RECT_F, source_w: f32, source_h: f32) -> D2D_RECT_F {
    if source_w <= 0.0 || source_h <= 0.0 {
        return container;
    }
    let width = container.right - container.left;
    let height = container.bottom - container.top;
    let scale = (width / source_w).min(height / source_h);
    let fitted_w = source_w * scale;
    let fitted_h = source_h * scale;
    let left = container.left + (width - fitted_w) / 2.0;
    let top = container.top + (height - fitted_h) / 2.0;
    D2D_RECT_F {
        left,
        top,
        right: left + fitted_w,
        bottom: top + fitted_h,
    }
}

pub unsafe fn draw(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    format: &IDWriteTextFormat,
    dock: &Dock,
    icons: &[Option<ID2D1Bitmap1>],
) {
    let frame = dock.frame();
    let base = BASE_ICON * dock.dpi;
    let theme = dock.theme.visual();

    dc.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));

    // --- dock pill (frosted dark, floating, rounded) ---
    let (l, t, r, b) = frame.pill;
    let pill = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: l,
            top: t,
            right: r,
            bottom: b,
        },
        radiusX: 22.0 * dock.dpi,
        radiusY: 22.0 * dock.dpi,
    };
    brush.SetColor(&color(
        theme.pill_rgb.0,
        theme.pill_rgb.1,
        theme.pill_rgb.2,
        theme.pill_alpha,
    ));
    dc.FillRoundedRectangle(&pill, brush);
    brush.SetColor(&color(1.0, 1.0, 1.0, theme.border_alpha));
    dc.DrawRoundedRectangle(&pill, brush, 1.0 * dock.dpi, None);

    // --- icons: one scale transform each, then a vector tile + emoji ---
    for ic in &frame.icons {
        let item = &dock.items[ic.idx];

        // `presence` (0..1) animates a slot appearing/disappearing: the icon scales
        // up from / down to nothing, fades by the same factor, and a new one rises
        // gently into place. Skip the degenerate fully-collapsed frame.
        let p = ic.presence;
        if p <= 0.001 {
            continue;
        }

        // The divider is a static separator: draw a line, fading with presence.
        if item.role == ItemRole::Divider {
            dc.SetTransform(&IDENTITY);
            draw_divider(
                dc,
                brush,
                ic.cx,
                frame.baseline,
                base,
                p,
                theme.divider_alpha,
            );
            continue;
        }

        // Scale about the icon's bottom-center (magnify * presence), lift by the
        // click-hop, and sink slightly while not fully present so it rises in/out.
        let rise = (1.0 - p) * ENTER_RISE * base;
        dc.SetTransform(&scale_about(
            ic.scale * p,
            ic.cx,
            frame.baseline,
            -ic.bounce_y + rise,
        ));

        let tile = D2D_RECT_F {
            left: ic.cx - base / 2.0,
            top: frame.baseline - base,
            right: ic.cx + base / 2.0,
            bottom: frame.baseline,
        };

        // The Start button is drawn as a crisp 4-pane vector glyph.
        if item.role == ItemRole::Start {
            draw_start(dc, brush, tile);
            continue;
        }

        // The Control button is drawn as a crisp "sliders" vector glyph.
        if item.role == ItemRole::Control {
            draw_control(dc, brush, tile);
            continue;
        }

        // The Drawer button is drawn as a crisp 3x3 "app grid" vector glyph.
        if item.role == ItemRole::Drawer {
            draw_drawer(dc, brush, tile);
            continue;
        }

        match icons.get(ic.idx).and_then(|o| o.as_ref()) {
            // Real application icon: draw the bitmap (transform scales it crisply).
            Some(bmp) => {
                let size = bmp.GetSize();
                let dest = fit_rect(tile, size.width, size.height);
                dc.DrawBitmap(
                    bmp,
                    Some(&dest),
                    p,
                    D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                    None,
                    None,
                );
            }
            // Fallback: colored rounded tile + emoji glyph.
            None => {
                let rr = D2D1_ROUNDED_RECT {
                    rect: tile,
                    radiusX: base * CORNER,
                    radiusY: base * CORNER,
                };
                brush.SetColor(&color(item.color.0, item.color.1, item.color.2, p));
                dc.FillRoundedRectangle(&rr, brush);
                brush.SetColor(&color(1.0, 1.0, 1.0, 0.97 * p));
                let wide: Vec<u16> = item.glyph.encode_utf16().collect();
                dc.DrawText(
                    &wide,
                    format,
                    &tile,
                    brush,
                    D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
    }

    // Running indicators: a small dot under any app that has open windows — both
    // pinned apps that are running (merged) and the right-side open windows. Drawn
    // unscaled (IDENTITY) so it stays a subtle, constant-size cue under the icon.
    dc.SetTransform(&IDENTITY);
    let running_dot = RunningDotStyle {
        rgb: theme.dot_rgb,
        alpha: theme.dot_alpha,
    };
    for ic in &frame.icons {
        let indicator_presence = ic.running_presence * ic.presence;
        if indicator_presence <= 0.02 {
            continue;
        }
        draw_running_dot(
            dc,
            brush,
            ic.cx,
            frame.baseline,
            base,
            indicator_presence,
            running_dot,
        );
    }
    dc.SetTransform(&IDENTITY);
}

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

/// A soft vertical line separating pinned apps from open windows.
unsafe fn draw_divider(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    cx: f32,
    baseline: f32,
    base: f32,
    presence: f32,
    alpha: f32,
) {
    let half = (base * 0.022).max(1.0); // ~2px line, scales gently with DPI
                                        // Grow the line in from the baseline as the slot opens, so it doesn't pop.
    let height = base * 0.72 * presence;
    let line = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: cx - half,
            top: baseline - base * 0.10 - height,
            right: cx + half,
            bottom: baseline - base * 0.10,
        },
        radiusX: half,
        radiusY: half,
    };
    brush.SetColor(&color(1.0, 1.0, 1.0, alpha * presence));
    dc.FillRoundedRectangle(&line, brush);
}

/// The Start button: a 2x2 grid of rounded panes (Windows-logo feel), drawn as
/// vectors so it stays crisp under magnification.
unsafe fn draw_start(dc: &ID2D1DeviceContext, brush: &ID2D1SolidColorBrush, tile: D2D_RECT_F) {
    let size = tile.right - tile.left;
    let pad = size * 0.20;
    let gap = size * 0.12;
    let inner_l = tile.left + pad;
    let inner_t = tile.top + pad;
    let cell = (size - 2.0 * pad - gap) / 2.0;
    let radius = cell * 0.20;
    brush.SetColor(&color(0.26, 0.60, 0.98, 1.0));
    for (col, row) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
        let left = inner_l + col * (cell + gap);
        let top = inner_t + row * (cell + gap);
        let pane = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left,
                top,
                right: left + cell,
                bottom: top + cell,
            },
            radiusX: radius,
            radiusY: radius,
        };
        dc.FillRoundedRectangle(&pane, brush);
    }
}

/// The Control button: two horizontal slider tracks with offset knobs (a "control
/// center / sliders" feel), drawn as vectors so it stays crisp under magnification.
unsafe fn draw_control(dc: &ID2D1DeviceContext, brush: &ID2D1SolidColorBrush, tile: D2D_RECT_F) {
    let size = tile.right - tile.left;
    let pad = size * 0.26;
    let left = tile.left + pad;
    let right = tile.right - pad;
    let track_h = size * 0.05;
    let knob_r = size * 0.10;
    // (vertical position as a fraction of the tile, knob position along the track)
    for (fy, kx) in [(0.40, 0.66_f32), (0.62, 0.34_f32)] {
        let y = tile.top + size * fy;
        let track = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left,
                top: y - track_h,
                right,
                bottom: y + track_h,
            },
            radiusX: track_h,
            radiusY: track_h,
        };
        brush.SetColor(&color(0.92, 0.94, 0.98, 0.85));
        dc.FillRoundedRectangle(&track, brush);
        let knob_x = left + (right - left) * kx;
        brush.SetColor(&color(1.0, 1.0, 1.0, 1.0));
        dc.FillEllipse(
            &D2D1_ELLIPSE {
                point: D2D_POINT_2F { x: knob_x, y },
                radiusX: knob_r,
                radiusY: knob_r,
            },
            brush,
        );
    }
}

/// The Drawer button: a 3x3 grid of rounded squares (an "app drawer / Launchpad"
/// feel), drawn as vectors so it stays crisp under magnification.
unsafe fn draw_drawer(dc: &ID2D1DeviceContext, brush: &ID2D1SolidColorBrush, tile: D2D_RECT_F) {
    let size = tile.right - tile.left;
    let pad = size * 0.24;
    let cell = (size - 2.0 * pad) / 3.0;
    let dot = cell * 0.40; // half-extent of each rounded square
    brush.SetColor(&color(0.92, 0.94, 0.98, 0.92));
    for row in 0..3 {
        for col in 0..3 {
            let cx = tile.left + pad + cell * (col as f32 + 0.5);
            let cy = tile.top + pad + cell * (row as f32 + 0.5);
            let pane = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: cx - dot,
                    top: cy - dot,
                    right: cx + dot,
                    bottom: cy + dot,
                },
                radiusX: dot * 0.42,
                radiusY: dot * 0.42,
            };
            dc.FillRoundedRectangle(&pane, brush);
        }
    }
}

/// A small "running" dot centered under an icon, in the pill's bottom padding.
unsafe fn draw_running_dot(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    cx: f32,
    baseline: f32,
    base: f32,
    presence: f32,
    style: RunningDotStyle,
) {
    let radius = (base * 0.045).max(2.0);
    let y = baseline + base * 0.09; // just below the icon, inside the pill padding
    brush.SetColor(&color(
        style.rgb.0,
        style.rgb.1,
        style.rgb.2,
        style.alpha * presence,
    ));
    dc.FillEllipse(
        &D2D1_ELLIPSE {
            point: D2D_POINT_2F { x: cx, y },
            radiusX: radius,
            radiusY: radius,
        },
        brush,
    );
}

/// Uniform scale `s` about anchor (ax, ay) — bottom-center, so icons grow upward —
/// plus a vertical translate `dy` (used for the click hop).
fn scale_about(s: f32, ax: f32, ay: f32, dy: f32) -> Matrix3x2 {
    Matrix3x2 {
        M11: s,
        M12: 0.0,
        M21: 0.0,
        M22: s,
        M31: ax * (1.0 - s),
        M32: ay * (1.0 - s) + dy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_wide_thumbnail_without_distortion() {
        let rect = fit_rect(
            D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: 100.0,
                bottom: 100.0,
            },
            200.0,
            100.0,
        );
        assert_eq!(rect.left, 0.0);
        assert_eq!(rect.right, 100.0);
        assert_eq!(rect.top, 25.0);
        assert_eq!(rect.bottom, 75.0);
    }
}
