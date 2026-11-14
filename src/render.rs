//! Direct2D drawing: the translucent pill + magnified icon tiles + emoji glyphs.
//! Only transforms change per frame; geometry is vector so it stays crisp at any scale.

use windows::core::Interface;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;

use crate::agent_state::AgentState;
use crate::dock::{Dock, ItemRole, CORNER, ENTER_RISE};
use crate::theme::{AgentTheme, ThemePreset};

/// Reused GPU material brushes: no gradient resources are allocated per frame.
pub struct MaterialBrushes {
    sheen: ID2D1LinearGradientBrush,
    shimmer: ID2D1LinearGradientBrush,
    badge: IDWriteTextFormat,
}

impl MaterialBrushes {
    pub unsafe fn new(dc: &ID2D1DeviceContext) -> windows::core::Result<Self> {
        let target: ID2D1RenderTarget = dc.cast()?;
        let stops = target.CreateGradientStopCollection(
            &[
                D2D1_GRADIENT_STOP {
                    position: 0.0,
                    color: color(1.0, 1.0, 1.0, 0.20),
                },
                D2D1_GRADIENT_STOP {
                    position: 1.0,
                    color: color(1.0, 1.0, 1.0, 0.0),
                },
            ],
            D2D1_GAMMA_2_2,
            D2D1_EXTEND_MODE_CLAMP,
        )?;
        let sheen = dc.CreateLinearGradientBrush(
            &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                startPoint: D2D_POINT_2F { x: 0.0, y: 0.0 },
                endPoint: D2D_POINT_2F { x: 0.0, y: 1.0 },
            },
            None,
            &stops,
        )?;
        let shimmer_stops = target.CreateGradientStopCollection(
            &[
                D2D1_GRADIENT_STOP {
                    position: 0.0,
                    color: color(0.88, 0.97, 0.93, 0.0),
                },
                D2D1_GRADIENT_STOP {
                    position: 0.36,
                    color: color(0.88, 0.97, 0.93, 0.0),
                },
                D2D1_GRADIENT_STOP {
                    position: 0.50,
                    color: color(0.88, 0.97, 0.93, 0.14),
                },
                D2D1_GRADIENT_STOP {
                    position: 0.64,
                    color: color(0.88, 0.97, 0.93, 0.0),
                },
                D2D1_GRADIENT_STOP {
                    position: 1.0,
                    color: color(0.88, 0.97, 0.93, 0.0),
                },
            ],
            D2D1_GAMMA_2_2,
            D2D1_EXTEND_MODE_CLAMP,
        )?;
        let shimmer = dc.CreateLinearGradientBrush(
            &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                startPoint: D2D_POINT_2F { x: 0.0, y: 0.0 },
                endPoint: D2D_POINT_2F { x: 1.0, y: 1.0 },
            },
            None,
            &shimmer_stops,
        )?;
        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let badge = factory.CreateTextFormat(
            windows::core::w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            11.0,
            windows::core::w!(""),
        )?;
        badge.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        badge.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        Ok(Self {
            sheen,
            shimmer,
            badge,
        })
    }
}

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
    materials: &MaterialBrushes,
    backdrop_enabled: bool,
    dock: &Dock,
    icons: &[Option<ID2D1Bitmap1>],
) {
    let frame = dock.frame();
    let base = dock.base_icon();
    let theme = dock.theme.visual();

    dc.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));

    // --- floating material; icons keep their original colors ---
    let (l, t, r, b) = frame.pill;
    let pill = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: l,
            top: t,
            right: r,
            bottom: b,
        },
        radiusX: theme.pill_radius * dock.dpi,
        radiusY: theme.pill_radius * dock.dpi,
    };
    brush.SetColor(&color(
        theme.pill_rgb.0,
        theme.pill_rgb.1,
        theme.pill_rgb.2,
        if dock.theme == ThemePreset::Glass && backdrop_enabled {
            dock.preferences.opacity as f32 / 100.0
        } else {
            dock.theme.dock_alpha(backdrop_enabled)
        },
    ));
    dc.FillRoundedRectangle(&pill, brush);
    if dock.theme == ThemePreset::Glass {
        materials.sheen.SetStartPoint(D2D_POINT_2F { x: l, y: t });
        materials.sheen.SetEndPoint(D2D_POINT_2F { x: l, y: b });
        dc.FillRoundedRectangle(&pill, &materials.sheen);
    }
    brush.SetColor(&color(1.0, 1.0, 1.0, theme.border_alpha));
    dc.DrawRoundedRectangle(&pill, brush, 1.0 * dock.dpi, None);
    let palette = dock.theme.agent_palette();
    draw_agent_zone_capsule(dc, brush, &frame, dock, base, theme.pill_radius, &palette);

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

        // Dividers are static separators: draw single or double line, fading with presence.
        if item.role.is_divider() {
            dc.SetTransform(&IDENTITY);
            let ctx = IndicatorCtx {
                dc,
                brush,
                cx: ic.cx,
                baseline: frame.baseline,
                base,
                presence: p,
            };
            if item.role == ItemRole::AgentDivider {
                draw_agent_divider(
                    &ctx,
                    palette.capsule_rgb,
                    (palette.capsule_border_alpha * 1.45).clamp(0.16, 0.45),
                );
            } else {
                draw_divider(&ctx, theme.divider_rgb, theme.divider_alpha);
            }
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
            draw_control(dc, brush, tile, theme.control_rgb);
            continue;
        }

        // The Drawer button is drawn as a crisp 3x3 "app grid" vector glyph.
        if item.role == ItemRole::Drawer {
            draw_drawer(dc, brush, tile, theme.control_rgb);
            continue;
        }

        if item.role == ItemRole::Overflow {
            brush.SetColor(&color(
                theme.control_rgb.0,
                theme.control_rgb.1,
                theme.control_rgb.2,
                p,
            ));
            for offset in [-0.22, 0.0, 0.22] {
                dc.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: D2D_POINT_2F {
                            x: ic.cx + base * offset,
                            y: frame.baseline - base * 0.45,
                        },
                        radiusX: base * 0.045,
                        radiusY: base * 0.045,
                    },
                    brush,
                );
            }
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
        if item
            .agent
            .as_ref()
            .is_some_and(|a| a.state == AgentState::Running)
        {
            draw_agent_shimmer(
                dc,
                brush,
                materials,
                tile,
                base,
                p,
                dock.shimmer_phase,
                &palette,
            );
        }
    }

    // Running indicators: a small dot under any active icon — regular running apps,
    // open Agents, and background Agents. Semantic Agent colors come from `palette`.
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
        let item = &dock.items[ic.idx];
        let (style, radius_scale) = match item.agent.as_ref().map(|a| a.state) {
            Some(AgentState::Waiting) => (
                RunningDotStyle {
                    rgb: palette.waiting_rgb,
                    alpha: 1.0,
                },
                1.18,
            ),
            Some(AgentState::Error) => (
                RunningDotStyle {
                    rgb: palette.error_rgb,
                    alpha: 1.0,
                },
                1.08,
            ),
            Some(AgentState::Running) => (
                RunningDotStyle {
                    rgb: palette.running_rgb,
                    alpha: 0.95,
                },
                1.0,
            ),
            Some(AgentState::Done) => (
                RunningDotStyle {
                    rgb: palette.done_rgb,
                    alpha: 0.92,
                },
                1.0,
            ),
            _ => (running_dot, 1.0),
        };
        let rise = (1.0 - ic.presence) * ENTER_RISE * base;
        draw_running_dot(
            &IndicatorCtx {
                dc,
                brush,
                cx: ic.cx,
                baseline: frame.baseline,
                base,
                presence: indicator_presence,
            },
            style,
            ic.bounce_y,
            rise,
            radius_scale,
        );
    }
    if let Some(x) = dock.insertion_x {
        brush.SetColor(&color(
            theme.dot_rgb.0,
            theme.dot_rgb.1,
            theme.dot_rgb.2,
            0.9,
        ));
        dc.DrawLine(
            D2D_POINT_2F {
                x,
                y: frame.baseline - base,
            },
            D2D_POINT_2F {
                x,
                y: frame.baseline,
            },
            brush,
            2.0 * dock.dpi,
            None,
        );
    }
    for ic in &frame.icons {
        let item = &dock.items[ic.idx];
        if dock
            .foreground
            .is_some_and(|fg| item.windows.iter().any(|w| w.hwnd == fg))
        {
            let y = frame.baseline + base * 0.09 - ic.bounce_y;
            brush.SetColor(&color(
                theme.dot_rgb.0,
                theme.dot_rgb.1,
                theme.dot_rgb.2,
                ic.presence,
            ));
            dc.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: ic.cx - base * 0.11,
                        right: ic.cx + base * 0.11,
                        top: y - base * 0.035,
                        bottom: y + base * 0.035,
                    },
                    radiusX: base * 0.035,
                    radiusY: base * 0.035,
                },
                brush,
            );
        }
        let badge_spec = if let Some(agent) = &item.agent {
            agent.badge.as_ref().map(|text| {
                let (fill, fg) = match agent.state {
                    AgentState::Waiting => (palette.waiting_rgb, palette.alert_text_rgb),
                    AgentState::Error => (palette.error_rgb, palette.alert_text_rgb),
                    _ if dock.theme == ThemePreset::Contrast => {
                        ((0.08, 0.08, 0.08), palette.badge_text_rgb)
                    }
                    _ if matches!(dock.theme, ThemePreset::Glass | ThemePreset::Macos) => (
                        (
                            (palette.capsule_rgb.0 * 0.35 + 0.65).min(0.95),
                            (palette.capsule_rgb.1 * 0.35 + 0.65).min(0.95),
                            (palette.capsule_rgb.2 * 0.35 + 0.65).min(0.95),
                        ),
                        palette.badge_text_rgb,
                    ),
                    _ => (
                        (
                            palette.capsule_rgb.0 * 0.42,
                            palette.capsule_rgb.1 * 0.42,
                            palette.capsule_rgb.2 * 0.42,
                        ),
                        palette.badge_text_rgb,
                    ),
                };
                (text.clone(), fill, fg)
            })
        } else if item.windows.len() > 1 {
            Some((
                item.windows.len().to_string(),
                theme.pill_rgb,
                theme.control_rgb,
            ))
        } else {
            None
        };
        if let Some((badge_text, fill_rgb, fg_rgb)) = badge_spec {
            let char_count = badge_text.chars().count();
            let width_factor = match char_count {
                0 | 1 => 0.34,
                2 => 0.44,
                _ => 0.52,
            };
            let right = ic.cx + base * 0.52;
            let bounds = D2D_RECT_F {
                left: right - base * width_factor,
                top: frame.baseline - base * 0.98,
                right,
                bottom: frame.baseline - base * 0.65,
            };
            let rr = D2D1_ROUNDED_RECT {
                rect: bounds,
                radiusX: base * 0.12,
                radiusY: base * 0.12,
            };
            brush.SetColor(&color(
                fill_rgb.0,
                fill_rgb.1,
                fill_rgb.2,
                0.96 * ic.presence,
            ));
            dc.FillRoundedRectangle(&rr, brush);
            brush.SetColor(&color(1.0, 1.0, 1.0, 0.22 * ic.presence));
            dc.DrawRoundedRectangle(&rr, brush, (0.75 * dock.dpi).max(0.75), None);
            brush.SetColor(&color(fg_rgb.0, fg_rgb.1, fg_rgb.2, ic.presence));
            let text: Vec<u16> = badge_text.encode_utf16().collect();
            let height = (bounds.bottom - bounds.top).max(1.0);
            let scale = height / 18.0;
            let draw_w = (bounds.right - bounds.left) / scale;
            dc.SetTransform(&Matrix3x2 {
                M11: scale,
                M12: 0.0,
                M21: 0.0,
                M22: scale,
                M31: bounds.left,
                M32: bounds.top,
            });
            dc.DrawText(
                &text,
                &materials.badge,
                &D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: draw_w,
                    bottom: 18.0,
                },
                brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
            dc.SetTransform(&IDENTITY);
        }
    }
    dc.SetTransform(&IDENTITY);
}

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

/// Per-frame geometry shared by the small under-icon cues (the divider and the
/// running dot): both draw at the same resting centre, baseline and base size,
/// scaled by presence. Grouping these six inputs keeps both draw calls' parameter
/// lists short (clippy `too_many_arguments`).
struct IndicatorCtx<'a> {
    dc: &'a ID2D1DeviceContext,
    brush: &'a ID2D1SolidColorBrush,
    cx: f32,
    baseline: f32,
    base: f32,
    presence: f32,
}

/// A soft vertical line separating pinned apps from open windows.
unsafe fn draw_divider(ctx: &IndicatorCtx, rgb: (f32, f32, f32), alpha: f32) {
    let half = (ctx.base * 0.022).max(1.0); // ~2px line, scales gently with DPI
                                            // Grow the line in from the baseline as the slot opens, so it doesn't pop.
    let height = ctx.base * 0.72 * ctx.presence;
    let line = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: ctx.cx - half,
            top: ctx.baseline - ctx.base * 0.10 - height,
            right: ctx.cx + half,
            bottom: ctx.baseline - ctx.base * 0.10,
        },
        radiusX: half,
        radiusY: half,
    };
    ctx.brush
        .SetColor(&color(rgb.0, rgb.1, rgb.2, alpha * ctx.presence));
    ctx.dc.FillRoundedRectangle(&line, ctx.brush);
}

/// A subtle double-line vertical separator framing the Agent Zone.
unsafe fn draw_agent_divider(ctx: &IndicatorCtx, rgb: (f32, f32, f32), alpha: f32) {
    let half = (ctx.base * 0.017).max(0.9);
    let offset = (ctx.base * 0.055).max(2.2);
    let height = ctx.base * 0.56 * ctx.presence;
    let bottom = ctx.baseline - ctx.base * 0.16;
    let top = bottom - height;
    ctx.brush
        .SetColor(&color(rgb.0, rgb.1, rgb.2, alpha * ctx.presence));
    for sign in [-1.0_f32, 1.0_f32] {
        let cx = ctx.cx + sign * offset;
        let line = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: cx - half,
                top,
                right: cx + half,
                bottom,
            },
            radiusX: half,
            radiusY: half,
        };
        ctx.dc.FillRoundedRectangle(&line, ctx.brush);
    }
}

/// Subtle translucent micro-capsule rendered behind Agent Zone icons inside the dock pill.
unsafe fn draw_agent_zone_capsule(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    frame: &crate::dock::Frame,
    dock: &Dock,
    base: f32,
    pill_radius: f32,
    palette: &AgentTheme,
) {
    let mut min_left = f32::INFINITY;
    let mut max_right = f32::NEG_INFINITY;
    for ic in &frame.icons {
        if ic.presence <= 0.05 || !dock.items[ic.idx].is_agent() {
            continue;
        }
        let half_w = 0.5 * base * ic.scale * ic.presence;
        min_left = min_left.min(ic.cx - half_w);
        max_right = max_right.max(ic.cx + half_w);
    }
    if min_left >= max_right {
        return;
    }
    let (pl, pt, pr, pb) = frame.pill;
    let layout_dpi = dock.layout_dpi();
    let pad_x = 5.0 * layout_dpi;
    let pad_y = 3.5 * layout_dpi;
    let left = (min_left - pad_x).max(pl + 3.0 * layout_dpi);
    let right = (max_right + pad_x).min(pr - 3.0 * layout_dpi);
    let top = pt + pad_y;
    let bottom = pb - pad_y;
    if right <= left || bottom <= top {
        return;
    }
    let radius = ((bottom - top) * 0.34).min((pill_radius - 2.0).max(6.0) * layout_dpi);
    let zone = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left,
            top,
            right,
            bottom,
        },
        radiusX: radius,
        radiusY: radius,
    };
    brush.SetColor(&color(
        palette.capsule_rgb.0,
        palette.capsule_rgb.1,
        palette.capsule_rgb.2,
        palette.capsule_alpha,
    ));
    dc.FillRoundedRectangle(&zone, brush);
    brush.SetColor(&color(
        palette.capsule_rgb.0,
        palette.capsule_rgb.1,
        palette.capsule_rgb.2,
        palette.capsule_border_alpha,
    ));
    dc.DrawRoundedRectangle(&zone, brush, 0.9 * layout_dpi, None);
}

/// Restrained diagonal shimmer + thin mint rim drawn on `Running` Agent icons.
#[allow(clippy::too_many_arguments)]
unsafe fn draw_agent_shimmer(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    materials: &MaterialBrushes,
    tile: D2D_RECT_F,
    base: f32,
    presence: f32,
    shimmer_phase: Option<f32>,
    palette: &AgentTheme,
) {
    let rr = D2D1_ROUNDED_RECT {
        rect: tile,
        radiusX: base * CORNER,
        radiusY: base * CORNER,
    };
    let mut phase_boost = 0.0f32;
    if let Some(phase) = shimmer_phase {
        phase_boost = (phase * std::f32::consts::PI).sin().max(0.0);
        let sweep = -0.35 + 1.70 * phase;
        materials.shimmer.SetStartPoint(D2D_POINT_2F {
            x: tile.left + (sweep - 0.50) * base,
            y: tile.top + (sweep - 0.50) * base,
        });
        materials.shimmer.SetEndPoint(D2D_POINT_2F {
            x: tile.left + (sweep + 0.50) * base,
            y: tile.top + (sweep + 0.50) * base,
        });
        materials.shimmer.SetOpacity(presence);
        dc.FillRoundedRectangle(&rr, &materials.shimmer);
    }
    let blend = 0.45 * phase_boost;
    let rim = (
        palette.running_rgb.0 + (palette.shimmer_rgb.0 - palette.running_rgb.0) * blend,
        palette.running_rgb.1 + (palette.shimmer_rgb.1 - palette.running_rgb.1) * blend,
        palette.running_rgb.2 + (palette.shimmer_rgb.2 - palette.running_rgb.2) * blend,
    );
    brush.SetColor(&color(
        rim.0,
        rim.1,
        rim.2,
        (0.30 + 0.22 * phase_boost) * presence,
    ));
    dc.DrawRoundedRectangle(&rr, brush, (base * 0.026).max(1.0), None);
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
unsafe fn draw_control(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    tile: D2D_RECT_F,
    rgb: (f32, f32, f32),
) {
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
        brush.SetColor(&color(rgb.0, rgb.1, rgb.2, 0.72));
        dc.FillRoundedRectangle(&track, brush);
        let knob_x = left + (right - left) * kx;
        brush.SetColor(&color(rgb.0, rgb.1, rgb.2, 1.0));
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
unsafe fn draw_drawer(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    tile: D2D_RECT_F,
    rgb: (f32, f32, f32),
) {
    let size = tile.right - tile.left;
    let pad = size * 0.24;
    let cell = (size - 2.0 * pad) / 3.0;
    let dot = cell * 0.40; // half-extent of each rounded square
    brush.SetColor(&color(rgb.0, rgb.1, rgb.2, 0.92));
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
/// The dot follows the icon's bounce and enter-rise offsets so it stays visually
/// attached to the icon instead of being left behind during click/birth animations.
unsafe fn draw_running_dot(
    ctx: &IndicatorCtx,
    style: RunningDotStyle,
    bounce_y: f32,
    rise: f32,
    radius_scale: f32,
) {
    let radius = (ctx.base * 0.045 * radius_scale).max(2.0);
    // Icon moves up by bounce_y and down by rise; the dot tracks it identically.
    let y = ctx.baseline + ctx.base * 0.09 - bounce_y + rise;
    ctx.brush.SetColor(&color(
        style.rgb.0,
        style.rgb.1,
        style.rgb.2,
        style.alpha * ctx.presence,
    ));
    ctx.dc.FillEllipse(
        &D2D1_ELLIPSE {
            point: D2D_POINT_2F { x: ctx.cx, y },
            radiusX: radius,
            radiusY: radius,
        },
        ctx.brush,
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
