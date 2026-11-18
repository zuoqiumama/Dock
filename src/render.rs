//! Direct2D drawing: translucent materials + magnified icon tiles + emoji glyphs.
//! Only transforms change per frame; geometry is vector so it stays crisp at any scale.

use windows::core::Interface;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;

use crate::agent_state::AgentState;
use crate::dock::{gap_for, Dock, ItemRole, CORNER, ENTER_RISE, PILL_PAD_X};
use crate::theme::{rgb8, AgentTheme, DockStyle, ThemePreset};

/// Reused GPU material brushes: no gradient resources are allocated per frame.
pub struct MaterialBrushes {
    factory: ID2D1Factory,
    sheen: ID2D1LinearGradientBrush,
    shimmer: ID2D1LinearGradientBrush,
    hairline_light: ID2D1LinearGradientBrush,
    hairline_shadow: ID2D1LinearGradientBrush,
    rim_light: ID2D1LinearGradientBrush,
    rim_dark: ID2D1LinearGradientBrush,
    shelf_deck: ID2D1LinearGradientBrush,
    badge: IDWriteTextFormat,
}

unsafe fn create_linear_brush(
    dc: &ID2D1DeviceContext,
    target: &ID2D1RenderTarget,
    stops: &[D2D1_GRADIENT_STOP],
    start: D2D_POINT_2F,
    end: D2D_POINT_2F,
) -> windows::core::Result<ID2D1LinearGradientBrush> {
    let collection =
        target.CreateGradientStopCollection(stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)?;
    dc.CreateLinearGradientBrush(
        &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
            startPoint: start,
            endPoint: end,
        },
        None,
        &collection,
    )
}

impl MaterialBrushes {
    pub unsafe fn new(
        dc: &ID2D1DeviceContext,
        factory: &ID2D1Factory,
    ) -> windows::core::Result<Self> {
        let target: ID2D1RenderTarget = dc.cast()?;
        let v0 = D2D_POINT_2F { x: 0.0, y: 0.0 };
        let v1 = D2D_POINT_2F { x: 0.0, y: 1.0 };
        let h1 = D2D_POINT_2F { x: 1.0, y: 0.0 };
        let d1 = D2D_POINT_2F { x: 1.0, y: 1.0 };

        let sheen = create_linear_brush(
            dc,
            &target,
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
            v0,
            v1,
        )?;
        let shimmer = create_linear_brush(
            dc,
            &target,
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
            v0,
            d1,
        )?;
        let hairline_stops = |rgb: (f32, f32, f32)| {
            [
                D2D1_GRADIENT_STOP {
                    position: 0.0,
                    color: color(rgb.0, rgb.1, rgb.2, 0.0),
                },
                D2D1_GRADIENT_STOP {
                    position: 0.14,
                    color: color(rgb.0, rgb.1, rgb.2, 1.0),
                },
                D2D1_GRADIENT_STOP {
                    position: 0.86,
                    color: color(rgb.0, rgb.1, rgb.2, 1.0),
                },
                D2D1_GRADIENT_STOP {
                    position: 1.0,
                    color: color(rgb.0, rgb.1, rgb.2, 0.0),
                },
            ]
        };
        let hairline_light =
            create_linear_brush(dc, &target, &hairline_stops((1.0, 1.0, 1.0)), v0, h1)?;
        let hairline_shadow =
            create_linear_brush(dc, &target, &hairline_stops((0.0, 0.0, 0.0)), v0, h1)?;
        let rim_light = create_linear_brush(
            dc,
            &target,
            &[
                D2D1_GRADIENT_STOP {
                    position: 0.0,
                    color: color(1.0, 1.0, 1.0, 0.78),
                },
                D2D1_GRADIENT_STOP {
                    position: 0.35,
                    color: color(1.0, 1.0, 1.0, 0.24),
                },
                D2D1_GRADIENT_STOP {
                    position: 1.0,
                    color: color(1.0, 1.0, 1.0, 0.14),
                },
            ],
            v0,
            v1,
        )?;
        let rim_dark = create_linear_brush(
            dc,
            &target,
            &[
                D2D1_GRADIENT_STOP {
                    position: 0.0,
                    color: color(1.0, 1.0, 1.0, 0.28),
                },
                D2D1_GRADIENT_STOP {
                    position: 0.35,
                    color: color(1.0, 1.0, 1.0, 0.10),
                },
                D2D1_GRADIENT_STOP {
                    position: 1.0,
                    color: color(1.0, 1.0, 1.0, 0.05),
                },
            ],
            v0,
            v1,
        )?;
        let shelf_deck = create_linear_brush(
            dc,
            &target,
            &[
                D2D1_GRADIENT_STOP {
                    position: 0.0,
                    color: color(1.0, 1.0, 1.0, 0.22),
                },
                D2D1_GRADIENT_STOP {
                    position: 0.45,
                    color: color(1.0, 1.0, 1.0, 0.06),
                },
                D2D1_GRADIENT_STOP {
                    position: 0.48,
                    color: color(1.0, 1.0, 1.0, 0.0),
                },
                D2D1_GRADIENT_STOP {
                    position: 1.0,
                    color: color(0.0, 0.0, 0.0, 0.08),
                },
            ],
            v0,
            v1,
        )?;
        let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let badge = dwrite.CreateTextFormat(
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
            factory: factory.clone(),
            sheen,
            shimmer,
            hairline_light,
            hairline_shadow,
            rim_light,
            rim_dark,
            shelf_deck,
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct IslandSpan {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    has_agent: bool,
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

fn inset_rect(rect: D2D_RECT_F, dx: f32, dy: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: rect.left + dx,
        top: rect.top + dy,
        right: (rect.right - dx).max(rect.left + dx),
        bottom: (rect.bottom - dy).max(rect.top + dy),
    }
}

fn mix_rgb(a: (f32, f32, f32), b: (f32, f32, f32), t: f32) -> (f32, f32, f32) {
    let t = t.clamp(0.0, 1.0);
    (
        a.0 + (b.0 - a.0) * t,
        a.1 + (b.1 - a.1) * t,
        a.2 + (b.2 - a.2) * t,
    )
}

fn container_alpha(dock: &Dock, backdrop_enabled: bool) -> f32 {
    match dock.style {
        DockStyle::Pill => {
            if dock.theme == ThemePreset::Glass && backdrop_enabled {
                dock.preferences.opacity as f32 / 100.0
            } else {
                dock.theme.dock_alpha(backdrop_enabled)
            }
        }
        DockStyle::Hairline => 0.0,
        DockStyle::Islands => {
            if dock.theme == ThemePreset::Glass {
                (dock.preferences.opacity as f32 / 100.0).max(0.68)
            } else if dock.theme == ThemePreset::Macos {
                0.76
            } else {
                dock.theme.dock_alpha(false)
            }
        }
        DockStyle::Shelf => {
            if !backdrop_enabled && dock.theme.is_light() {
                0.92
            } else {
                match dock.theme {
                    ThemePreset::Glass => (dock.preferences.opacity as f32 / 100.0).max(0.46),
                    ThemePreset::Macos => 0.68,
                    ThemePreset::Compact => 0.84,
                    ThemePreset::Solid | ThemePreset::Contrast => 1.0,
                }
            }
        }
        DockStyle::Tiles => match dock.theme {
            ThemePreset::Glass => (dock.preferences.opacity as f32 / 100.0).max(0.74),
            ThemePreset::Macos => 0.80,
            ThemePreset::Compact => 0.90,
            ThemePreset::Solid | ThemePreset::Contrast => 1.0,
        },
        DockStyle::Fluent => {
            if !backdrop_enabled && dock.theme.is_light() {
                0.92
            } else {
                match dock.theme {
                    ThemePreset::Glass => (dock.preferences.opacity as f32 / 100.0).max(0.48),
                    ThemePreset::Macos => 0.62,
                    ThemePreset::Compact => 0.78,
                    ThemePreset::Solid | ThemePreset::Contrast => 1.0,
                }
            }
        }
    }
}

fn agent_zone_bounds(frame: &crate::dock::Frame, dock: &Dock, base: f32) -> Option<(f32, f32)> {
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
    (min_left < max_right).then_some((min_left, max_right))
}

fn island_spans(frame: &crate::dock::Frame, dock: &Dock, base: f32) -> Vec<IslandSpan> {
    let (_, pt, _, pb) = frame.pill;
    let layout_dpi = dock.layout_dpi();
    let outer_pad = PILL_PAD_X * layout_dpi;
    let inner_pad = if dock.theme == ThemePreset::Compact {
        8.0
    } else {
        11.0
    } * layout_dpi;

    let mut groups: Vec<(usize, usize, Vec<&crate::dock::IconFrame>)> = Vec::new();
    let mut i = 0;
    while i < frame.icons.len() {
        if dock.items[frame.icons[i].idx].role.is_divider() {
            i += 1;
            continue;
        }
        let start = i;
        while i < frame.icons.len() && !dock.items[frame.icons[i].idx].role.is_divider() {
            i += 1;
        }
        let visible: Vec<&crate::dock::IconFrame> = frame.icons[start..i]
            .iter()
            .filter(|ic| ic.presence > 0.02)
            .collect();
        if !visible.is_empty() {
            groups.push((start, i - 1, visible));
        }
    }

    let max_div_between = |from_excl: usize, to_excl: usize| -> f32 {
        frame.icons[from_excl + 1..to_excl]
            .iter()
            .filter(|ic| dock.items[ic.idx].role.is_divider())
            .map(|ic| ic.presence)
            .fold(0.0_f32, f32::max)
    };

    let mut spans = Vec::with_capacity(groups.len());
    for (g_idx, (_, _, visible)) in groups.iter().enumerate() {
        let first = visible[0];
        let last = visible[visible.len() - 1];
        let left_div = if g_idx > 0 {
            max_div_between(groups[g_idx - 1].1, groups[g_idx].0)
        } else {
            0.0
        };
        let right_div = if g_idx + 1 < groups.len() {
            max_div_between(groups[g_idx].1, groups[g_idx + 1].0)
        } else {
            0.0
        };
        let left_pad = outer_pad + (inner_pad - outer_pad) * left_div;
        let right_pad = outer_pad + (inner_pad - outer_pad) * right_div;
        let left = first.cx - 0.5 * base * first.scale * first.presence - left_pad;
        let right = last.cx + 0.5 * base * last.scale * last.presence + right_pad;
        let has_agent = visible.iter().any(|ic| dock.items[ic.idx].is_agent());
        if right > left {
            spans.push(IslandSpan {
                left,
                top: pt,
                right,
                bottom: pb,
                has_agent,
            });
        }
    }
    spans
}

unsafe fn draw_soft_shadow(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    rect: D2D_RECT_F,
    radius: f32,
    dpi: f32,
    alpha: f32,
) {
    for (spread, dy, factor) in [(1.5, 1.5, 0.58), (3.0, 3.2, 0.30)] {
        let s = spread * dpi;
        let offset = dy * dpi;
        let shadow = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: rect.left - s,
                top: rect.top - s * 0.4 + offset,
                right: rect.right + s,
                bottom: rect.bottom + s * 0.8 + offset,
            },
            radiusX: radius + s,
            radiusY: radius + s,
        };
        brush.SetColor(&color(0.0, 0.0, 0.0, alpha * factor));
        dc.FillRoundedRectangle(&shadow, brush);
    }
}

unsafe fn draw_specular_rounded_border(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    materials: &MaterialBrushes,
    rect: D2D_RECT_F,
    radius: f32,
    dpi: f32,
    theme: ThemePreset,
) {
    let stroke = 1.0 * dpi;
    let rr = D2D1_ROUNDED_RECT {
        rect: inset_rect(rect, stroke * 0.5, stroke * 0.5),
        radiusX: (radius - stroke * 0.5).max(1.0),
        radiusY: (radius - stroke * 0.5).max(1.0),
    };
    if theme == ThemePreset::Contrast {
        let visual = theme.visual();
        brush.SetColor(&color(1.0, 1.0, 1.0, visual.border_alpha));
        dc.DrawRoundedRectangle(&rr, brush, stroke, None);
        return;
    }
    let rim = if theme.is_light() {
        &materials.rim_light
    } else {
        &materials.rim_dark
    };
    rim.SetStartPoint(D2D_POINT_2F {
        x: rr.rect.left,
        y: rr.rect.top,
    });
    rim.SetEndPoint(D2D_POINT_2F {
        x: rr.rect.left,
        y: rr.rect.bottom,
    });
    rim.SetOpacity(1.0);
    dc.DrawRoundedRectangle(&rr, rim, stroke, None);
}

unsafe fn build_top_rounded_rect(
    factory: &ID2D1Factory,
    rect: D2D_RECT_F,
    radius: f32,
) -> Option<ID2D1PathGeometry> {
    let r = radius.clamp(
        0.0,
        ((rect.right - rect.left) * 0.5).min(rect.bottom - rect.top),
    );
    let geometry = factory.CreatePathGeometry().ok()?;
    let sink = geometry.Open().ok()?;
    sink.BeginFigure(
        D2D_POINT_2F {
            x: rect.left,
            y: rect.bottom,
        },
        D2D1_FIGURE_BEGIN_FILLED,
    );
    sink.AddLine(D2D_POINT_2F {
        x: rect.left,
        y: rect.top + r,
    });
    if r > 0.5 {
        sink.AddArc(&D2D1_ARC_SEGMENT {
            point: D2D_POINT_2F {
                x: rect.left + r,
                y: rect.top,
            },
            size: D2D_SIZE_F {
                width: r,
                height: r,
            },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: D2D1_ARC_SIZE_SMALL,
        });
    }
    sink.AddLine(D2D_POINT_2F {
        x: rect.right - r,
        y: rect.top,
    });
    if r > 0.5 {
        sink.AddArc(&D2D1_ARC_SEGMENT {
            point: D2D_POINT_2F {
                x: rect.right,
                y: rect.top + r,
            },
            size: D2D_SIZE_F {
                width: r,
                height: r,
            },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: D2D1_ARC_SIZE_SMALL,
        });
    }
    sink.AddLine(D2D_POINT_2F {
        x: rect.right,
        y: rect.bottom,
    });
    sink.EndFigure(D2D1_FIGURE_END_CLOSED);
    let _ = sink.Close();
    Some(geometry)
}

unsafe fn build_top_ridge_path(
    factory: &ID2D1Factory,
    rect: D2D_RECT_F,
    radius: f32,
) -> Option<ID2D1PathGeometry> {
    let r = radius.clamp(
        0.0,
        ((rect.right - rect.left) * 0.5).min(rect.bottom - rect.top),
    );
    let geometry = factory.CreatePathGeometry().ok()?;
    let sink = geometry.Open().ok()?;
    sink.BeginFigure(
        D2D_POINT_2F {
            x: rect.left,
            y: rect.top + r,
        },
        D2D1_FIGURE_BEGIN_HOLLOW,
    );
    if r > 0.5 {
        sink.AddArc(&D2D1_ARC_SEGMENT {
            point: D2D_POINT_2F {
                x: rect.left + r,
                y: rect.top,
            },
            size: D2D_SIZE_F {
                width: r,
                height: r,
            },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: D2D1_ARC_SIZE_SMALL,
        });
    }
    sink.AddLine(D2D_POINT_2F {
        x: rect.right - r,
        y: rect.top,
    });
    if r > 0.5 {
        sink.AddArc(&D2D1_ARC_SEGMENT {
            point: D2D_POINT_2F {
                x: rect.right,
                y: rect.top + r,
            },
            size: D2D_SIZE_F {
                width: r,
                height: r,
            },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: D2D1_ARC_SIZE_SMALL,
        });
    }
    sink.EndFigure(D2D1_FIGURE_END_OPEN);
    let _ = sink.Close();
    Some(geometry)
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_container(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    materials: &MaterialBrushes,
    backdrop_enabled: bool,
    dock: &Dock,
    frame: &crate::dock::Frame,
    base: f32,
    palette: &AgentTheme,
) {
    let theme = dock.theme.visual();
    let (l, t, r, b) = frame.pill;
    let rect = D2D_RECT_F {
        left: l,
        top: t,
        right: r,
        bottom: b,
    };
    let radius = dock.style.corner_radius(&theme) * dock.dpi;

    match dock.style {
        DockStyle::Pill => {
            let pill = D2D1_ROUNDED_RECT {
                rect,
                radiusX: radius,
                radiusY: radius,
            };
            brush.SetColor(&color(
                theme.pill_rgb.0,
                theme.pill_rgb.1,
                theme.pill_rgb.2,
                container_alpha(dock, backdrop_enabled),
            ));
            dc.FillRoundedRectangle(&pill, brush);
            if dock.theme == ThemePreset::Glass {
                materials.sheen.SetStartPoint(D2D_POINT_2F { x: l, y: t });
                materials.sheen.SetEndPoint(D2D_POINT_2F { x: l, y: b });
                materials.sheen.SetOpacity(1.0);
                dc.FillRoundedRectangle(&pill, &materials.sheen);
            }
            brush.SetColor(&color(1.0, 1.0, 1.0, theme.border_alpha));
            dc.DrawRoundedRectangle(&pill, brush, 1.0 * dock.dpi, None);
            draw_agent_zone_capsule(dc, brush, frame, dock, base, theme.pill_radius, palette);
        }
        DockStyle::Hairline => {
            let y = t + 1.0 * dock.dpi;
            if dock.theme == ThemePreset::Contrast {
                let inset = 6.0 * dock.dpi;
                brush.SetColor(&color(0.0, 0.0, 0.0, 0.65));
                dc.DrawLine(
                    D2D_POINT_2F {
                        x: l + inset,
                        y: y + 1.2 * dock.dpi,
                    },
                    D2D_POINT_2F {
                        x: r - inset,
                        y: y + 1.2 * dock.dpi,
                    },
                    brush,
                    2.0 * dock.dpi,
                    None,
                );
                brush.SetColor(&color(
                    theme.dot_rgb.0,
                    theme.dot_rgb.1,
                    theme.dot_rgb.2,
                    0.92,
                ));
                dc.DrawLine(
                    D2D_POINT_2F { x: l + inset, y },
                    D2D_POINT_2F { x: r - inset, y },
                    brush,
                    1.5 * dock.dpi,
                    None,
                );
            } else {
                materials.hairline_shadow.SetStartPoint(D2D_POINT_2F {
                    x: l,
                    y: y + 1.0 * dock.dpi,
                });
                materials.hairline_shadow.SetEndPoint(D2D_POINT_2F {
                    x: r,
                    y: y + 1.0 * dock.dpi,
                });
                materials.hairline_shadow.SetOpacity(0.28);
                dc.DrawLine(
                    D2D_POINT_2F {
                        x: l,
                        y: y + 1.0 * dock.dpi,
                    },
                    D2D_POINT_2F {
                        x: r,
                        y: y + 1.0 * dock.dpi,
                    },
                    &materials.hairline_shadow,
                    1.4 * dock.dpi,
                    None,
                );

                materials
                    .hairline_light
                    .SetStartPoint(D2D_POINT_2F { x: l, y });
                materials
                    .hairline_light
                    .SetEndPoint(D2D_POINT_2F { x: r, y });
                materials
                    .hairline_light
                    .SetOpacity(if dock.theme.is_light() { 0.74 } else { 0.62 });
                dc.DrawLine(
                    D2D_POINT_2F { x: l, y },
                    D2D_POINT_2F { x: r, y },
                    &materials.hairline_light,
                    1.25 * dock.dpi,
                    None,
                );
            }
            if let Some((min_left, max_right)) = agent_zone_bounds(frame, dock, base) {
                let pad = 5.0 * dock.layout_dpi();
                let seg_l = (min_left - pad).max(l + 6.0 * dock.layout_dpi());
                let seg_r = (max_right + pad).min(r - 6.0 * dock.layout_dpi());
                if seg_r > seg_l {
                    let has_waiting = frame.icons.iter().any(|ic| {
                        dock.items[ic.idx]
                            .agent
                            .as_ref()
                            .is_some_and(|a| a.state == AgentState::Waiting)
                    });
                    let accent = if has_waiting {
                        palette.waiting_rgb
                    } else {
                        palette.running_rgb
                    };
                    brush.SetColor(&color(accent.0, accent.1, accent.2, 0.72));
                    dc.DrawLine(
                        D2D_POINT_2F { x: seg_l, y },
                        D2D_POINT_2F { x: seg_r, y },
                        brush,
                        1.8 * dock.dpi,
                        None,
                    );
                }
            }
        }
        DockStyle::Islands => {
            let alpha = container_alpha(dock, backdrop_enabled);
            let spans = island_spans(frame, dock, base);
            for span in spans {
                let island_rect = D2D_RECT_F {
                    left: span.left,
                    top: span.top,
                    right: span.right,
                    bottom: span.bottom,
                };
                let island_r = radius.min((span.bottom - span.top) * 0.45);
                let rr = D2D1_ROUNDED_RECT {
                    rect: island_rect,
                    radiusX: island_r,
                    radiusY: island_r,
                };
                draw_soft_shadow(dc, brush, island_rect, island_r, dock.dpi, 0.24);
                brush.SetColor(&color(
                    theme.pill_rgb.0,
                    theme.pill_rgb.1,
                    theme.pill_rgb.2,
                    alpha,
                ));
                dc.FillRoundedRectangle(&rr, brush);
                if dock.theme != ThemePreset::Contrast {
                    materials.sheen.SetStartPoint(D2D_POINT_2F {
                        x: span.left,
                        y: span.top,
                    });
                    materials.sheen.SetEndPoint(D2D_POINT_2F {
                        x: span.left,
                        y: span.bottom,
                    });
                    materials
                        .sheen
                        .SetOpacity(if dock.theme.is_light() { 0.65 } else { 0.24 });
                    dc.FillRoundedRectangle(&rr, &materials.sheen);
                }
                draw_specular_rounded_border(
                    dc,
                    brush,
                    materials,
                    island_rect,
                    island_r,
                    dock.dpi,
                    dock.theme,
                );
            }
            draw_agent_zone_capsule(
                dc,
                brush,
                frame,
                dock,
                base,
                dock.style.corner_radius(&theme),
                palette,
            );
        }
        DockStyle::Shelf => {
            if let Some(shelf_geo) = build_top_rounded_rect(&materials.factory, rect, radius) {
                let haze = D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: l + 4.0 * dock.dpi,
                        top: t - 2.5 * dock.dpi,
                        right: r - 4.0 * dock.dpi,
                        bottom: t + radius,
                    },
                    radiusX: radius,
                    radiusY: radius,
                };
                brush.SetColor(&color(0.0, 0.0, 0.0, 0.14));
                dc.FillRoundedRectangle(&haze, brush);

                brush.SetColor(&color(
                    theme.pill_rgb.0,
                    theme.pill_rgb.1,
                    theme.pill_rgb.2,
                    container_alpha(dock, backdrop_enabled),
                ));
                dc.FillGeometry(&shelf_geo, brush, None);

                if dock.theme != ThemePreset::Contrast {
                    materials
                        .shelf_deck
                        .SetStartPoint(D2D_POINT_2F { x: l, y: t });
                    materials
                        .shelf_deck
                        .SetEndPoint(D2D_POINT_2F { x: l, y: b });
                    materials.shelf_deck.SetOpacity(if dock.theme.is_light() {
                        0.85
                    } else {
                        0.50
                    });
                    dc.FillGeometry(&shelf_geo, &materials.shelf_deck, None);
                }

                let ridge_rect = inset_rect(rect, 0.5 * dock.dpi, 0.6 * dock.dpi);
                if let Some(ridge) = build_top_ridge_path(
                    &materials.factory,
                    ridge_rect,
                    (radius - 0.5 * dock.dpi).max(1.0),
                ) {
                    let ridge_alpha = if dock.theme == ThemePreset::Contrast {
                        0.85
                    } else if dock.theme.is_light() {
                        0.82
                    } else {
                        0.36
                    };
                    brush.SetColor(&color(1.0, 1.0, 1.0, ridge_alpha));
                    dc.DrawGeometry(&ridge, brush, 1.35 * dock.dpi, None);
                }
            }
            draw_agent_zone_capsule(
                dc,
                brush,
                frame,
                dock,
                base,
                dock.style.corner_radius(&theme),
                palette,
            );
        }
        DockStyle::Tiles => {}
        DockStyle::Fluent => {
            let rr = D2D1_ROUNDED_RECT {
                rect,
                radiusX: radius,
                radiusY: radius,
            };
            draw_soft_shadow(dc, brush, rect, radius, dock.dpi, 0.25);
            brush.SetColor(&color(
                theme.pill_rgb.0,
                theme.pill_rgb.1,
                theme.pill_rgb.2,
                container_alpha(dock, backdrop_enabled),
            ));
            dc.FillRoundedRectangle(&rr, brush);
            if dock.theme != ThemePreset::Contrast {
                materials.sheen.SetStartPoint(D2D_POINT_2F { x: l, y: t });
                materials.sheen.SetEndPoint(D2D_POINT_2F { x: l, y: b });
                materials
                    .sheen
                    .SetOpacity(if dock.theme.is_light() { 0.45 } else { 0.18 });
                dc.FillRoundedRectangle(&rr, &materials.sheen);
            }
            draw_specular_rounded_border(dc, brush, materials, rect, radius, dock.dpi, dock.theme);
            if dock.theme != ThemePreset::Contrast && r - l > radius * 2.2 {
                brush.SetColor(&color(
                    1.0,
                    1.0,
                    1.0,
                    if dock.theme.is_light() { 0.42 } else { 0.16 },
                ));
                dc.DrawLine(
                    D2D_POINT_2F {
                        x: l + radius,
                        y: t + 1.0 * dock.dpi,
                    },
                    D2D_POINT_2F {
                        x: r - radius,
                        y: t + 1.0 * dock.dpi,
                    },
                    brush,
                    0.8 * dock.dpi,
                    None,
                );
            }
            draw_agent_zone_capsule(dc, brush, frame, dock, base, 8.0, palette);
        }
    }
}

#[derive(Clone, Copy)]
struct IconMotion {
    scale: f32,
    press_scale: f32,
    dy: f32,
}

fn icon_motion(dock: &Dock, ic: &crate::dock::IconFrame, base: f32) -> IconMotion {
    let p = ic.presence;
    let rise = (1.0 - p) * ENTER_RISE * base;
    let press_scale = if dock.style == DockStyle::Pill {
        1.0
    } else {
        1.0 - 0.055 * ic.press
    };
    let press_sink = if dock.style == DockStyle::Pill {
        0.0
    } else {
        1.2 * dock.dpi * ic.press
    };
    IconMotion {
        scale: ic.scale * p * press_scale,
        press_scale,
        dy: -ic.bounce_y + rise + press_sink,
    }
}

#[derive(Clone, Copy, Debug)]
struct BadgeGeometry {
    bounds: D2D_RECT_F,
    radius: f32,
    stroke_width: f32,
    shadow_spread: f32,
    text_scale: f32,
    text_width: f32,
}

/// Position and scale a corner badge so it stays anchored to the icon's
/// visual top-right corner across hover magnification, click bounce/press,
/// and enter/exit transitions.
fn badge_geometry(
    dock: &Dock,
    frame: &crate::dock::Frame,
    ic: &crate::dock::IconFrame,
    base: f32,
    char_count: usize,
) -> Option<BadgeGeometry> {
    if ic.presence <= 0.02 {
        return None;
    }
    let motion = icon_motion(dock, ic, base);
    // Grow sub-linearly on hover magnification so badges move with the icon
    // without becoming oversized at high magnification settings.
    let badge_scale = ic.scale.max(1.0).sqrt() * ic.presence * motion.press_scale;
    let width_factor = match char_count {
        0 | 1 => 0.34,
        2 => 0.44,
        _ => 0.52,
    };
    let badge_w = base * width_factor * badge_scale;
    let badge_h = base * 0.33 * badge_scale;
    if badge_h <= 1.5 || badge_w <= 1.5 {
        return None;
    }
    let right = ic.cx + base * (0.50 * motion.scale + 0.02 * badge_scale);
    let top = frame.baseline - base * motion.scale + motion.dy + base * 0.02 * badge_scale;
    let bounds = D2D_RECT_F {
        left: right - badge_w,
        top,
        right,
        bottom: top + badge_h,
    };
    let radius = (base * 0.12 * badge_scale).min(badge_h * 0.5);
    let text_scale = badge_h / 18.0;
    let scale_root = badge_scale.sqrt();
    Some(BadgeGeometry {
        bounds,
        radius,
        stroke_width: (0.75 * dock.dpi * scale_root).max(0.75),
        shadow_spread: (0.5 * dock.dpi * scale_root).max(0.4),
        text_scale,
        text_width: badge_w / text_scale,
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_keycap_tile(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    materials: &MaterialBrushes,
    dock: &Dock,
    ic: &crate::dock::IconFrame,
    base: f32,
    baseline: f32,
    is_foreground: bool,
    palette: &AgentTheme,
) {
    let p = ic.presence;
    let motion = icon_motion(dock, ic, base);
    let s = motion.scale;
    let layout_dpi = dock.layout_dpi();
    let pad_x = (gap_for(dock.theme) * 0.24).clamp(2.2, 3.6) * layout_dpi;
    let pad_top = 4.5 * layout_dpi;
    let bottom_ext = (6.5 + 1.2 * (ic.scale - 1.0).max(0.0)) * layout_dpi * p;
    let y_base = baseline + motion.dy;
    let keycap = D2D_RECT_F {
        left: ic.cx - (base * 0.5 + pad_x) * s,
        top: y_base - (base + pad_top) * s,
        right: ic.cx + (base * 0.5 + pad_x) * s,
        bottom: y_base + bottom_ext,
    };
    let radius = (base * 0.28 * s.sqrt()).max(4.0);
    let visual = dock.theme.visual();
    let item = &dock.items[ic.idx];

    draw_soft_shadow(dc, brush, keycap, radius, dock.dpi, 0.22 * p);

    let mut fill_rgb = visual.pill_rgb;
    if item.is_agent() {
        fill_rgb = mix_rgb(fill_rgb, palette.capsule_rgb, 0.14);
    }
    if is_foreground {
        fill_rgb = mix_rgb(fill_rgb, visual.dot_rgb, 0.12);
    }
    if ic.hover > 0.01 {
        let target = if dock.theme.is_light() {
            mix_rgb((1.0, 1.0, 1.0), palette.running_rgb, 0.16)
        } else {
            (0.92, 0.95, 1.0)
        };
        fill_rgb = mix_rgb(fill_rgb, target, 0.22 * ic.hover);
    }
    let alpha = (container_alpha(dock, false) + 0.12 * ic.hover).min(1.0) * p;
    let rr = D2D1_ROUNDED_RECT {
        rect: keycap,
        radiusX: radius,
        radiusY: radius,
    };
    brush.SetColor(&color(fill_rgb.0, fill_rgb.1, fill_rgb.2, alpha));
    dc.FillRoundedRectangle(&rr, brush);

    if dock.theme != ThemePreset::Contrast {
        materials.sheen.SetStartPoint(D2D_POINT_2F {
            x: keycap.left,
            y: keycap.top,
        });
        materials.sheen.SetEndPoint(D2D_POINT_2F {
            x: keycap.left,
            y: keycap.bottom,
        });
        materials
            .sheen
            .SetOpacity((if dock.theme.is_light() { 0.50 } else { 0.20 }) * p);
        dc.FillRoundedRectangle(&rr, &materials.sheen);
    }

    draw_specular_rounded_border(dc, brush, materials, keycap, radius, dock.dpi, dock.theme);
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
    let palette = dock.theme.agent_palette();

    dc.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));

    draw_container(
        dc,
        brush,
        materials,
        backdrop_enabled,
        dock,
        &frame,
        base,
        &palette,
    );

    // --- icons: one scale transform each, then a vector tile + emoji ---
    for ic in &frame.icons {
        let item = &dock.items[ic.idx];
        let p = ic.presence;
        if p <= 0.001 {
            continue;
        }

        if item.role.is_divider() {
            if dock.style == DockStyle::Islands {
                continue;
            }
            dc.SetTransform(&IDENTITY);
            let ctx = IndicatorCtx {
                dc,
                brush,
                cx: ic.cx,
                baseline: frame.baseline,
                base,
                presence: p,
            };
            let height_ratio: f32 = match dock.style {
                DockStyle::Hairline | DockStyle::Tiles => 0.44,
                _ => 0.72,
            };
            if item.role == ItemRole::AgentDivider {
                draw_agent_divider(
                    &ctx,
                    palette.capsule_rgb,
                    (palette.capsule_border_alpha * 1.45).clamp(0.16, 0.45),
                    height_ratio.min(0.56),
                );
            } else {
                if dock.style == DockStyle::Hairline {
                    draw_divider_shadow(&ctx, height_ratio);
                }
                draw_divider(&ctx, theme.divider_rgb, theme.divider_alpha, height_ratio);
            }
            continue;
        }

        let is_foreground = dock
            .foreground
            .is_some_and(|fg| item.windows.iter().any(|w| w.hwnd == fg));

        if dock.style == DockStyle::Tiles {
            dc.SetTransform(&IDENTITY);
            draw_keycap_tile(
                dc,
                brush,
                materials,
                dock,
                ic,
                base,
                frame.baseline,
                is_foreground,
                &palette,
            );
        }

        let motion = icon_motion(dock, ic, base);
        dc.SetTransform(&scale_about(motion.scale, ic.cx, frame.baseline, motion.dy));

        let raw_tile = D2D_RECT_F {
            left: ic.cx - base / 2.0,
            top: frame.baseline - base,
            right: ic.cx + base / 2.0,
            bottom: frame.baseline,
        };
        let tile = if dock.style == DockStyle::Tiles {
            let inset = base * 0.08;
            D2D_RECT_F {
                left: raw_tile.left + inset,
                top: raw_tile.top + inset * 0.4,
                right: raw_tile.right - inset,
                bottom: raw_tile.bottom - inset * 1.6,
            }
        } else {
            raw_tile
        };

        if matches!(
            item.role,
            ItemRole::Start
                | ItemRole::Control
                | ItemRole::Drawer
                | ItemRole::Overflow
                | ItemRole::SystemTray
        ) {
            draw_system_button(dc, brush, dock, item.role, tile, p);
            continue;
        }

        match icons.get(ic.idx).and_then(|o| o.as_ref()) {
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
            None => {
                let tile_w = tile.right - tile.left;
                let rr = D2D1_ROUNDED_RECT {
                    rect: tile,
                    radiusX: tile_w * CORNER,
                    radiusY: tile_w * CORNER,
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

    // Running indicators & foreground marks + badges
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
        let is_foreground = dock
            .foreground
            .is_some_and(|fg| item.windows.iter().any(|w| w.hwnd == fg));
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
        match dock.style {
            DockStyle::Pill => {
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
            DockStyle::Islands => {
                if is_foreground {
                    let y = frame.baseline + base * 0.09 - ic.bounce_y + rise;
                    draw_pill_indicator(
                        dc,
                        brush,
                        ic.cx,
                        y,
                        base * 0.28,
                        (base * 0.068).max(2.6),
                        style.rgb,
                        indicator_presence,
                        0.0,
                    );
                } else {
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
            }
            DockStyle::Hairline => {
                let y = frame.baseline + base * 0.10 - ic.bounce_y + rise;
                let w = if is_foreground {
                    base * 0.40
                } else {
                    base * 0.20 * radius_scale
                };
                let indicator_rgb = if item.agent.is_none() && dock.theme.is_light() {
                    mix_rgb(style.rgb, (0.94, 0.97, 0.96), 0.65)
                } else {
                    style.rgb
                };
                draw_pill_indicator(
                    dc,
                    brush,
                    ic.cx,
                    y,
                    w,
                    (base * 0.065).max(2.6),
                    indicator_rgb,
                    style.alpha * indicator_presence,
                    0.34 * indicator_presence,
                );
            }
            DockStyle::Shelf | DockStyle::Fluent => {
                let y = frame.baseline + base * 0.09 - ic.bounce_y + rise;
                let w = if is_foreground {
                    base * 0.36
                } else {
                    base * 0.16 * radius_scale
                };
                draw_pill_indicator(
                    dc,
                    brush,
                    ic.cx,
                    y,
                    w,
                    (base * 0.064).max(2.5),
                    style.rgb,
                    style.alpha * indicator_presence,
                    0.0,
                );
            }
            DockStyle::Tiles => {
                let motion = icon_motion(dock, ic, base);
                let y = frame.baseline
                    + motion.dy
                    + (3.2 + 0.6 * (ic.scale - 1.0).max(0.0)) * dock.layout_dpi() * ic.presence;
                let w = if is_foreground {
                    base * 0.48
                } else {
                    base * 0.26 * radius_scale
                } * ic.scale;
                draw_pill_indicator(
                    dc,
                    brush,
                    ic.cx,
                    y,
                    w,
                    (2.8 * dock.dpi).max(2.2),
                    style.rgb,
                    style.alpha * indicator_presence,
                    0.0,
                );
            }
        }
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
        if dock.style == DockStyle::Pill
            && dock
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
                    _ if dock.theme.is_light() => (
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
            if let Some(geom) = badge_geometry(dock, &frame, ic, base, char_count) {
                let shadow_rr = D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: geom.bounds.left - geom.shadow_spread,
                        top: geom.bounds.top + geom.shadow_spread * 0.5,
                        right: geom.bounds.right + geom.shadow_spread,
                        bottom: geom.bounds.bottom + geom.shadow_spread * 2.0,
                    },
                    radiusX: geom.radius + geom.shadow_spread * 0.6,
                    radiusY: geom.radius + geom.shadow_spread * 0.6,
                };
                brush.SetColor(&color(0.0, 0.0, 0.0, 0.18 * ic.presence));
                dc.FillRoundedRectangle(&shadow_rr, brush);

                let rr = D2D1_ROUNDED_RECT {
                    rect: geom.bounds,
                    radiusX: geom.radius,
                    radiusY: geom.radius,
                };
                brush.SetColor(&color(
                    fill_rgb.0,
                    fill_rgb.1,
                    fill_rgb.2,
                    0.96 * ic.presence,
                ));
                dc.FillRoundedRectangle(&rr, brush);
                let border_alpha = if dock.theme == ThemePreset::Contrast {
                    0.62
                } else {
                    0.22
                };
                brush.SetColor(&color(1.0, 1.0, 1.0, border_alpha * ic.presence));
                dc.DrawRoundedRectangle(&rr, brush, geom.stroke_width, None);
                brush.SetColor(&color(fg_rgb.0, fg_rgb.1, fg_rgb.2, ic.presence));
                let text: Vec<u16> = badge_text.encode_utf16().collect();
                dc.SetTransform(&Matrix3x2 {
                    M11: geom.text_scale,
                    M12: 0.0,
                    M21: 0.0,
                    M22: geom.text_scale,
                    M31: geom.bounds.left,
                    M32: geom.bounds.top,
                });
                dc.DrawText(
                    &text,
                    &materials.badge,
                    &D2D_RECT_F {
                        left: 0.0,
                        top: 0.0,
                        right: geom.text_width,
                        bottom: 18.0,
                    },
                    brush,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
                dc.SetTransform(&IDENTITY);
            }
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

unsafe fn divider_line_rect(
    ctx: &IndicatorCtx,
    height_ratio: f32,
    half: f32,
    offset: f32,
) -> D2D1_ROUNDED_RECT {
    let height = ctx.base * height_ratio * ctx.presence;
    let center_offset = (0.72 - height_ratio) * 0.5 * ctx.base;
    let bottom = ctx.baseline - ctx.base * 0.10 - center_offset;
    D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: ctx.cx - half + offset,
            top: bottom - height + offset,
            right: ctx.cx + half + offset,
            bottom: bottom + offset,
        },
        radiusX: half,
        radiusY: half,
    }
}

unsafe fn draw_divider_shadow(ctx: &IndicatorCtx, height_ratio: f32) {
    let half = (ctx.base * 0.028).max(1.3);
    let line = divider_line_rect(ctx, height_ratio, half, 0.8);
    ctx.brush
        .SetColor(&color(0.0, 0.0, 0.0, 0.24 * ctx.presence));
    ctx.dc.FillRoundedRectangle(&line, ctx.brush);
}

/// A soft vertical line separating pinned apps from open windows.
unsafe fn draw_divider(ctx: &IndicatorCtx, rgb: (f32, f32, f32), alpha: f32, height_ratio: f32) {
    let half = (ctx.base * 0.022).max(1.0); // ~2px line, scales gently with DPI
    let line = divider_line_rect(ctx, height_ratio, half, 0.0);
    ctx.brush
        .SetColor(&color(rgb.0, rgb.1, rgb.2, alpha * ctx.presence));
    ctx.dc.FillRoundedRectangle(&line, ctx.brush);
}

/// A subtle double-line vertical separator framing the Agent Zone.
unsafe fn draw_agent_divider(
    ctx: &IndicatorCtx,
    rgb: (f32, f32, f32),
    alpha: f32,
    height_ratio: f32,
) {
    let half = (ctx.base * 0.017).max(0.9);
    let offset = (ctx.base * 0.055).max(2.2);
    let height = ctx.base * height_ratio * ctx.presence;
    let center_offset = (0.56 - height_ratio) * 0.5 * ctx.base;
    let bottom = ctx.baseline - ctx.base * 0.16 - center_offset;
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
    let Some((min_left, max_right)) = agent_zone_bounds(frame, dock, base) else {
        return;
    };
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
    let tile_w = tile.right - tile.left;
    let rr = D2D1_ROUNDED_RECT {
        rect: tile,
        radiusX: tile_w * CORNER,
        radiusY: tile_w * CORNER,
    };
    let mut phase_boost = 0.0f32;
    if let Some(phase) = shimmer_phase {
        phase_boost = (phase * std::f32::consts::PI).sin().max(0.0);
        let sweep = -0.35 + 1.70 * phase;
        materials.shimmer.SetStartPoint(D2D_POINT_2F {
            x: tile.left + (sweep - 0.50) * tile_w,
            y: tile.top + (sweep - 0.50) * tile_w,
        });
        materials.shimmer.SetEndPoint(D2D_POINT_2F {
            x: tile.left + (sweep + 0.50) * tile_w,
            y: tile.top + (sweep + 0.50) * tile_w,
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

#[derive(Clone, Copy, Debug)]
struct SystemButtonColors {
    primary: (f32, f32, f32),
    secondary: (f32, f32, f32),
    accent: (f32, f32, f32),
    detail: (f32, f32, f32),
}

fn system_button_colors(role: ItemRole, theme: ThemePreset) -> SystemButtonColors {
    if theme == ThemePreset::Contrast {
        return match role {
            ItemRole::Start => SystemButtonColors {
                primary: (1.0, 1.0, 1.0),
                secondary: (1.0, 1.0, 1.0),
                accent: (1.0, 1.0, 1.0),
                detail: (1.0, 1.0, 1.0),
            },
            ItemRole::Drawer => SystemButtonColors {
                primary: rgb8(255, 214, 10),
                secondary: (1.0, 1.0, 1.0),
                accent: (0.82, 0.82, 0.82),
                detail: (0.0, 0.0, 0.0),
            },
            ItemRole::Control => SystemButtonColors {
                primary: rgb8(255, 214, 10),
                secondary: (1.0, 1.0, 1.0),
                accent: (1.0, 1.0, 1.0),
                detail: (0.0, 0.0, 0.0),
            },
            _ => SystemButtonColors {
                primary: (1.0, 1.0, 1.0),
                secondary: rgb8(255, 214, 10),
                accent: (1.0, 1.0, 1.0),
                detail: (0.0, 0.0, 0.0),
            },
        };
    }

    match role {
        ItemRole::Start => SystemButtonColors {
            primary: rgb8(70, 162, 245),
            secondary: rgb8(98, 184, 255),
            accent: rgb8(50, 138, 228),
            detail: rgb8(124, 198, 255),
        },
        ItemRole::Drawer => SystemButtonColors {
            primary: rgb8(252, 176, 74),
            secondary: rgb8(64, 218, 190),
            accent: rgb8(138, 124, 234),
            detail: rgb8(254, 248, 236),
        },
        ItemRole::Control => SystemButtonColors {
            primary: rgb8(64, 218, 190),
            secondary: rgb8(138, 124, 234),
            accent: rgb8(242, 118, 86),
            detail: rgb8(254, 248, 236),
        },
        _ => SystemButtonColors {
            primary: rgb8(252, 176, 74),
            secondary: rgb8(64, 218, 190),
            accent: rgb8(138, 124, 234),
            detail: rgb8(254, 248, 236),
        },
    }
}

fn tile_subrect(tile: D2D_RECT_F, l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    let size = tile.right - tile.left;
    D2D_RECT_F {
        left: tile.left + size * l,
        top: tile.top + size * t,
        right: tile.left + size * r,
        bottom: tile.top + size * b,
    }
}

unsafe fn fill_flat_rr(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    rect: D2D_RECT_F,
    radius: f32,
    rgb: (f32, f32, f32),
    alpha: f32,
    hairline_shadow_dy: Option<f32>,
) {
    if let Some(dy) = hairline_shadow_dy {
        let shadow = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: rect.left,
                top: rect.top + dy,
                right: rect.right,
                bottom: rect.bottom + dy,
            },
            radiusX: radius,
            radiusY: radius,
        };
        brush.SetColor(&color(0.0, 0.0, 0.0, 0.14 * alpha));
        dc.FillRoundedRectangle(&shadow, brush);
    }
    brush.SetColor(&color(rgb.0, rgb.1, rgb.2, alpha));
    dc.FillRoundedRectangle(
        &D2D1_ROUNDED_RECT {
            rect,
            radiusX: radius,
            radiusY: radius,
        },
        brush,
    );
}

unsafe fn draw_system_button(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    dock: &Dock,
    role: ItemRole,
    tile: D2D_RECT_F,
    alpha: f32,
) {
    let size = tile.right - tile.left;
    let shadow_dy = (dock.style == DockStyle::Hairline && dock.theme != ThemePreset::Contrast)
        .then_some(size * 0.020);
    let colors = system_button_colors(role, dock.theme);
    match role {
        ItemRole::Start => draw_start(dc, brush, tile, colors, alpha, shadow_dy),
        ItemRole::Drawer => draw_drawer(dc, brush, dock.theme, tile, colors, alpha, shadow_dy),
        ItemRole::Control => draw_control(dc, brush, tile, colors, alpha, shadow_dy),
        ItemRole::Overflow => draw_overflow_dots(dc, brush, tile, colors, alpha, shadow_dy),
        ItemRole::SystemTray => draw_tray_chevron(dc, brush, tile, colors, alpha, shadow_dy),
        _ => {}
    }
}

unsafe fn draw_tray_chevron(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    tile: D2D_RECT_F,
    colors: SystemButtonColors,
    alpha: f32,
    shadow_dy: Option<f32>,
) {
    let size = tile.right - tile.left;
    let draw_chevron = |offset_y| {
        let point = |x, y| D2D_POINT_2F {
            x: tile.left + size * x,
            y: tile.top + size * y + offset_y,
        };
        let peak = point(0.50, 0.30);
        let stroke = (size * 0.10).max(1.0);
        dc.DrawLine(point(0.24, 0.55), peak, brush, stroke, None);
        dc.DrawLine(peak, point(0.76, 0.55), brush, stroke, None);
    };
    if let Some(offset) = shadow_dy {
        brush.SetColor(&color(0.0, 0.0, 0.0, alpha * 0.30));
        draw_chevron(offset);
    }
    let (r, g, b) = colors.primary;
    brush.SetColor(&color(r, g, b, alpha));
    draw_chevron(0.0);
    fill_flat_rr(
        dc,
        brush,
        tile_subrect(tile, 0.30, 0.64, 0.70, 0.73),
        size * 0.045,
        colors.secondary,
        alpha,
        shadow_dy,
    );
}

/// Borderless flat 2x2 Windows panes in a soft cerulean-sky palette.
unsafe fn draw_start(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    tile: D2D_RECT_F,
    colors: SystemButtonColors,
    alpha: f32,
    shadow_dy: Option<f32>,
) {
    let size = tile.right - tile.left;
    let pad = size * 0.16;
    let gap = size * 0.065;
    let cell = (size - 2.0 * pad - gap) / 2.0;
    let radius = cell * 0.18;
    let panes = [
        (0.0, 0.0, colors.primary),
        (1.0, 0.0, colors.secondary),
        (0.0, 1.0, colors.accent),
        (1.0, 1.0, colors.detail),
    ];
    for (col, row, rgb) in panes {
        let left = tile.left + pad + col * (cell + gap);
        let top = tile.top + pad + row * (cell + gap);
        let rect = D2D_RECT_F {
            left,
            top,
            right: left + cell,
            bottom: top + cell,
        };
        fill_flat_rr(dc, brush, rect, radius, rgb, alpha, shadow_dy);
    }
}

/// Borderless flat origami pocket/drawer matching FeatherDock's brand icon:
/// two pastel cards peeking from a warm peach drawer body with a pull handle.
unsafe fn draw_drawer(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    theme: ThemePreset,
    tile: D2D_RECT_F,
    colors: SystemButtonColors,
    alpha: f32,
    shadow_dy: Option<f32>,
) {
    let size = tile.right - tile.left;
    let lip_rgb = if theme == ThemePreset::Contrast {
        rgb8(200, 160, 0)
    } else {
        rgb8(242, 118, 86)
    };
    fill_flat_rr(
        dc,
        brush,
        tile_subrect(tile, 0.25, 0.14, 0.81, 0.56),
        size * 0.09,
        colors.accent,
        alpha,
        shadow_dy,
    );
    fill_flat_rr(
        dc,
        brush,
        tile_subrect(tile, 0.18, 0.25, 0.75, 0.66),
        size * 0.09,
        colors.secondary,
        alpha,
        None,
    );
    fill_flat_rr(
        dc,
        brush,
        tile_subrect(tile, 0.14, 0.39, 0.86, 0.84),
        size * 0.12,
        lip_rgb,
        alpha,
        shadow_dy,
    );
    fill_flat_rr(
        dc,
        brush,
        tile_subrect(tile, 0.14, 0.43, 0.86, 0.86),
        size * 0.12,
        colors.primary,
        alpha,
        None,
    );
    fill_flat_rr(
        dc,
        brush,
        tile_subrect(tile, 0.37, 0.60, 0.63, 0.68),
        size * 0.04,
        colors.detail,
        alpha,
        None,
    );
}

/// Borderless flat control switches: two pastel capsule toggles with cream knobs.
unsafe fn draw_control(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    tile: D2D_RECT_F,
    colors: SystemButtonColors,
    alpha: f32,
    shadow_dy: Option<f32>,
) {
    let size = tile.right - tile.left;
    let pad_x = size * 0.14;
    let left = tile.left + pad_x;
    let right = tile.right - pad_x;
    let pill_h = size * 0.27;
    let half_h = pill_h * 0.5;
    let gap = size * 0.12;
    let y0 = tile.top + (size - (2.0 * pill_h + gap)) * 0.5 + half_h;
    let knob_r = pill_h * 0.36;

    for (i, (y, track_rgb)) in [(y0, colors.primary), (y0 + pill_h + gap, colors.secondary)]
        .into_iter()
        .enumerate()
    {
        let track = D2D_RECT_F {
            left,
            top: y - half_h,
            right,
            bottom: y + half_h,
        };
        fill_flat_rr(dc, brush, track, half_h, track_rgb, alpha, shadow_dy);
        let knob_x = if i == 0 {
            right - half_h
        } else {
            left + half_h
        };
        brush.SetColor(&color(
            colors.detail.0,
            colors.detail.1,
            colors.detail.2,
            alpha,
        ));
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

/// Borderless flat overflow dots in FeatherDock's pastel trio.
unsafe fn draw_overflow_dots(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    tile: D2D_RECT_F,
    colors: SystemButtonColors,
    alpha: f32,
    shadow_dy: Option<f32>,
) {
    let size = tile.right - tile.left;
    let cx = (tile.left + tile.right) * 0.5;
    let cy = (tile.top + tile.bottom) * 0.5;
    let dot_r = size * 0.105;
    let dots = [
        (-0.28, colors.primary),
        (0.0, colors.secondary),
        (0.28, colors.accent),
    ];
    for (offset, rgb) in dots {
        let x = cx + size * offset;
        if let Some(dy) = shadow_dy {
            brush.SetColor(&color(0.0, 0.0, 0.0, 0.14 * alpha));
            dc.FillEllipse(
                &D2D1_ELLIPSE {
                    point: D2D_POINT_2F { x, y: cy + dy },
                    radiusX: dot_r,
                    radiusY: dot_r,
                },
                brush,
            );
        }
        brush.SetColor(&color(rgb.0, rgb.1, rgb.2, alpha));
        dc.FillEllipse(
            &D2D1_ELLIPSE {
                point: D2D_POINT_2F { x, y: cy },
                radiusX: dot_r,
                radiusY: dot_r,
            },
            brush,
        );
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_pill_indicator(
    dc: &ID2D1DeviceContext,
    brush: &ID2D1SolidColorBrush,
    cx: f32,
    y: f32,
    width: f32,
    height: f32,
    rgb: (f32, f32, f32),
    alpha: f32,
    shadow_alpha: f32,
) {
    let half_w = width * 0.5;
    let half_h = height * 0.5;
    if shadow_alpha > 0.01 {
        let shadow = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: cx - half_w - 0.8,
                top: y - half_h + 0.4,
                right: cx + half_w + 0.8,
                bottom: y + half_h + 1.2,
            },
            radiusX: half_h + 0.6,
            radiusY: half_h + 0.6,
        };
        brush.SetColor(&color(0.0, 0.0, 0.0, shadow_alpha));
        dc.FillRoundedRectangle(&shadow, brush);
    }
    let pill = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: cx - half_w,
            top: y - half_h,
            right: cx + half_w,
            bottom: y + half_h,
        },
        radiusX: half_h,
        radiusY: half_h,
    };
    brush.SetColor(&color(rgb.0, rgb.1, rgb.2, alpha));
    dc.FillRoundedRectangle(&pill, brush);
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
    use crate::content::ContentKind;
    use crate::dock::{AgentSlotInfo, DockItem};

    fn test_item(role: ItemRole, path: Option<&str>, agent: Option<&str>) -> DockItem {
        DockItem {
            label: "Test".into(),
            glyph: "A",
            color: (0.2, 0.4, 0.8),
            path: path.map(str::to_string),
            icon: None,
            kind: ContentKind::Application,
            role,
            hwnd: None,
            group_key: None,
            windows: Vec::new(),
            agent: agent.map(AgentSlotInfo::idle),
        }
    }

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

    #[test]
    fn islands_split_at_dividers_without_overlapping() {
        let items = vec![
            test_item(ItemRole::Start, None, None),
            test_item(ItemRole::Pinned, Some("C:\\App1.exe"), None),
            test_item(ItemRole::Divider, None, None),
            test_item(ItemRole::Running, None, None),
            test_item(ItemRole::AgentDivider, None, None),
            test_item(ItemRole::Pinned, Some("C:\\Codex.exe"), Some("codex")),
            test_item(ItemRole::Control, None, None),
        ];
        let mut dock = Dock::new(items, 1.0, 1200.0, 200.0, ThemePreset::Glass);
        dock.style = DockStyle::Islands;
        let frame = dock.frame();
        let spans = island_spans(&frame, &dock, dock.base_icon());

        assert_eq!(spans.len(), 3);
        assert!(!spans[0].has_agent);
        assert!(!spans[1].has_agent);
        assert!(spans[2].has_agent);
        for pair in spans.windows(2) {
            assert!(
                pair[0].right + 10.0 < pair[1].left,
                "islands should have a clear transparent gap: {:?} vs {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    fn approx_eq(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-3, "expected {b}, got {a}");
    }

    #[test]
    fn badge_matches_resting_corner_and_widths() {
        let items = vec![
            test_item(ItemRole::Start, None, None),
            test_item(ItemRole::Pinned, Some("C:\\App1.exe"), None),
            test_item(ItemRole::Control, None, None),
        ];
        let dock = Dock::new(items, 1.0, 1200.0, 200.0, ThemePreset::Glass);
        let frame = dock.frame();
        let base = dock.base_icon();
        let ic = &frame.icons[1];

        let one = badge_geometry(&dock, &frame, ic, base, 1).expect("visible badge");
        approx_eq(one.bounds.right, ic.cx + base * 0.52);
        approx_eq(one.bounds.top, frame.baseline - base * 0.98);
        approx_eq(one.bounds.bottom, frame.baseline - base * 0.65);
        approx_eq(one.bounds.right - one.bounds.left, base * 0.34);

        let two = badge_geometry(&dock, &frame, ic, base, 2).expect("visible badge");
        approx_eq(two.bounds.right - two.bounds.left, base * 0.44);

        let three = badge_geometry(&dock, &frame, ic, base, 3).expect("visible badge");
        approx_eq(three.bounds.right - three.bounds.left, base * 0.52);
    }

    #[test]
    fn badge_tracks_hover_magnification_and_stays_in_headroom() {
        let items = vec![
            test_item(ItemRole::Start, None, None),
            test_item(ItemRole::Pinned, Some("C:\\App1.exe"), None),
            test_item(ItemRole::Control, None, None),
        ];
        let prefs = crate::settings::Layout::default();
        let (w, h) = crate::dock::window_size_with_style(
            &items,
            1.5,
            prefs,
            ThemePreset::Glass,
            DockStyle::Tiles,
        );
        let mut dock = Dock::new(items, 1.5, w as f32, h as f32, ThemePreset::Glass);
        dock.style = DockStyle::Tiles;
        let frame = dock.frame();
        let base = dock.base_icon();

        let mut hovered = crate::dock::IconFrame {
            idx: frame.icons[1].idx,
            cx: frame.icons[1].cx + 12.0,
            scale: 1.9,
            bounce_y: 0.0,
            presence: 1.0,
            running_presence: 1.0,
            hover: 1.0,
            press: 0.0,
        };
        let rest = badge_geometry(&dock, &frame, &frame.icons[1], base, 2).unwrap();
        let mag = badge_geometry(&dock, &frame, &hovered, base, 2).unwrap();

        let icon_top = frame.baseline - base * 1.9;
        let icon_right = hovered.cx + base * 0.5 * 1.9;
        let badge_scale = 1.9_f32.sqrt();
        approx_eq(mag.bounds.top, icon_top + base * 0.02 * badge_scale);
        approx_eq(mag.bounds.right, icon_right + base * 0.02 * badge_scale);
        assert!(mag.bounds.bottom - mag.bounds.top > rest.bounds.bottom - rest.bounds.top);
        assert!(mag.bounds.top < rest.bounds.top - base * 0.8);

        hovered.bounce_y = crate::dock::BOUNCE_AMP * base;
        let bounced = badge_geometry(&dock, &frame, &hovered, base, 2).unwrap();
        assert!(
            bounced.bounds.top > 0.0,
            "magnified + bouncing badge clipped top edge: {}",
            bounced.bounds.top
        );
    }

    #[test]
    fn badge_tracks_bounce_presence_and_press() {
        let items = vec![
            test_item(ItemRole::Start, None, None),
            test_item(ItemRole::Pinned, Some("C:\\App1.exe"), None),
            test_item(ItemRole::Control, None, None),
        ];
        let mut dock = Dock::new(items, 1.0, 1200.0, 200.0, ThemePreset::Glass);
        let frame = dock.frame();
        let base = dock.base_icon();
        let base_ic = &frame.icons[1];

        let rest = badge_geometry(&dock, &frame, base_ic, base, 1).unwrap();

        let bouncing = crate::dock::IconFrame {
            idx: base_ic.idx,
            cx: base_ic.cx,
            scale: 1.0,
            bounce_y: 14.0,
            presence: 1.0,
            running_presence: 1.0,
            hover: 0.0,
            press: 0.0,
        };
        let bounced = badge_geometry(&dock, &frame, &bouncing, base, 1).unwrap();
        approx_eq(bounced.bounds.top, rest.bounds.top - 14.0);
        approx_eq(bounced.bounds.bottom, rest.bounds.bottom - 14.0);

        let entering = crate::dock::IconFrame {
            presence: 0.5,
            ..bouncing
        };
        let enter_geom = badge_geometry(&dock, &frame, &entering, base, 1).unwrap();
        let rise = 0.5 * ENTER_RISE * base;
        approx_eq(
            enter_geom.bounds.top,
            frame.baseline - base * 0.49 - 14.0 + rise,
        );
        approx_eq(
            enter_geom.bounds.bottom - enter_geom.bounds.top,
            (rest.bounds.bottom - rest.bounds.top) * 0.5,
        );

        let invisible = crate::dock::IconFrame {
            presence: 0.01,
            ..bouncing
        };
        assert!(badge_geometry(&dock, &frame, &invisible, base, 1).is_none());

        let pressed_ic = crate::dock::IconFrame {
            idx: base_ic.idx,
            cx: base_ic.cx,
            scale: 1.0,
            bounce_y: 0.0,
            presence: 1.0,
            running_presence: 1.0,
            hover: 1.0,
            press: 1.0,
        };
        let pill_press = badge_geometry(&dock, &frame, &pressed_ic, base, 1).unwrap();
        approx_eq(pill_press.bounds.top, rest.bounds.top);

        dock.style = DockStyle::Fluent;
        let fluent_press = badge_geometry(&dock, &frame, &pressed_ic, base, 1).unwrap();
        assert!(fluent_press.bounds.top > rest.bounds.top);
        assert!(
            fluent_press.bounds.bottom - fluent_press.bounds.top
                < rest.bounds.bottom - rest.bounds.top
        );
    }

    fn relative_luminance(rgb: (f32, f32, f32)) -> f32 {
        let lin = |c: f32| {
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(rgb.0) + 0.7152 * lin(rgb.1) + 0.0722 * lin(rgb.2)
    }

    fn contrast_ratio(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
        let la = relative_luminance(a);
        let lb = relative_luminance(b);
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn system_buttons_stay_distinct_and_visible_against_dark_and_contrast_backgrounds() {
        let dark_bg = rgb8(30, 30, 34);
        let contrast_bg = (0.0, 0.0, 0.0);
        let start = system_button_colors(ItemRole::Start, ThemePreset::Glass);
        let drawer = system_button_colors(ItemRole::Drawer, ThemePreset::Glass);
        let control = system_button_colors(ItemRole::Control, ThemePreset::Glass);
        assert_ne!(start.primary, drawer.primary);
        assert_ne!(drawer.primary, control.primary);

        for theme in ThemePreset::ALL {
            let bg = if theme == ThemePreset::Contrast {
                contrast_bg
            } else {
                dark_bg
            };
            for role in [
                ItemRole::Start,
                ItemRole::Drawer,
                ItemRole::Control,
                ItemRole::Overflow,
                ItemRole::SystemTray,
            ] {
                let c = system_button_colors(role, theme);
                let min_ratio = if theme == ThemePreset::Contrast {
                    8.0
                } else {
                    4.0
                };
                let ratio = contrast_ratio(c.primary, bg);
                assert!(
                    ratio >= min_ratio,
                    "primary color contrast too low ({ratio:.2}) in {theme:?}"
                );
            }
        }
    }
}
