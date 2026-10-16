use eframe::egui::epaint::text::VariationCoords;
use eframe::egui::{self, Color32, FontId, Painter, Pos2, Rect, Sense, Shape, Stroke, Vec2};
use std::sync::Arc;

// Surfaces, darkest to lightest. Warm brown-grey neutrals.
pub const SIDEBAR: Color32 = Color32::from_rgb(0x1C, 0x1B, 0x19);
pub const BG: Color32 = Color32::from_rgb(0x23, 0x22, 0x20);
pub const RAISED: Color32 = Color32::from_rgb(0x2A, 0x29, 0x26);
pub const SELECTED: Color32 = Color32::from_rgb(0x33, 0x31, 0x2D);
pub const LINE: Color32 = Color32::from_rgb(0x34, 0x32, 0x2E);
pub const HAIRLINE: Color32 = Color32::from_rgb(0x2B, 0x29, 0x26);
pub const RAIL: Color32 = Color32::from_rgb(0x3A, 0x38, 0x33);
pub const EDGE_HOVER: Color32 = Color32::from_rgb(0x4A, 0x46, 0x40);

// Text.
pub const INK: Color32 = Color32::from_rgb(0xEE, 0xEB, 0xE3);
pub const MUTED: Color32 = Color32::from_rgb(0xA8, 0xA3, 0x97);
pub const FAINT: Color32 = Color32::from_rgb(0x91, 0x8C, 0x81);
pub const DISABLED: Color32 = Color32::from_rgb(0x5E, 0x5A, 0x53);

// Accent (clay) and status.
pub const ACCENT: Color32 = Color32::from_rgb(0xD9, 0x77, 0x57);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0xE0, 0x8F, 0x75);
pub const ACCENT_PRESSED: Color32 = Color32::from_rgb(0xC6, 0x67, 0x48);
pub const ACCENT_INK: Color32 = Color32::from_rgb(0xE0, 0x8F, 0x75);
pub const ACCENT_TINT: Color32 = Color32::from_rgb(0x3E, 0x2F, 0x28);
pub const ACCENT_EDGE: Color32 = Color32::from_rgb(0x6A, 0x42, 0x31);
pub const WARNING: Color32 = Color32::from_rgb(0xE2, 0xB3, 0x5E);
pub const WARNING_TINT: Color32 = Color32::from_rgb(0x33, 0x29, 0x1C);
pub const WARNING_EDGE: Color32 = Color32::from_rgb(0x5C, 0x47, 0x26);

/// One text style: size, variable-font weight, letter spacing and family.
#[derive(Clone, Copy)]
pub struct Type {
    pub size: f32,
    pub weight: f32,
    pub tracking: f32,
    pub mono: bool,
}

pub const T_TITLE: Type = Type {
    size: 22.0,
    weight: 600.0,
    tracking: -0.2,
    mono: false,
};
pub const T_BRAND: Type = Type {
    size: 15.0,
    weight: 600.0,
    tracking: -0.15,
    mono: false,
};
pub const T_CARD: Type = Type {
    size: 14.0,
    weight: 600.0,
    tracking: 0.0,
    mono: false,
};
pub const T_BODY: Type = Type {
    size: 13.0,
    weight: 400.0,
    tracking: 0.0,
    mono: false,
};
pub const T_BODY_MEDIUM: Type = Type {
    size: 13.0,
    weight: 500.0,
    tracking: 0.0,
    mono: false,
};
pub const T_BODY_STRONG: Type = Type {
    size: 13.0,
    weight: 600.0,
    tracking: 0.0,
    mono: false,
};
pub const T_SMALL: Type = Type {
    size: 12.0,
    weight: 400.0,
    tracking: 0.0,
    mono: false,
};
pub const T_SMALL_MEDIUM: Type = Type {
    size: 12.0,
    weight: 500.0,
    tracking: 0.0,
    mono: false,
};
pub const T_LABEL: Type = Type {
    size: 11.0,
    weight: 600.0,
    tracking: 0.6,
    mono: false,
};
pub const T_NUM: Type = Type {
    size: 12.5,
    weight: 400.0,
    tracking: 0.0,
    mono: true,
};
pub const T_NUM_SMALL: Type = Type {
    size: 11.5,
    weight: 400.0,
    tracking: 0.0,
    mono: true,
};

/// Lay out one line of text. Long text is elided with an ellipsis.
pub fn layout_text(
    ui: &egui::Ui,
    text: &str,
    ty: Type,
    color: Color32,
    max_width: f32,
) -> Arc<egui::text::Galley> {
    layout_rich(ui, &[(text, ty, color)], max_width)
}

/// Lay out one line made of differently styled parts.
pub fn layout_rich(
    ui: &egui::Ui,
    parts: &[(&str, Type, Color32)],
    max_width: f32,
) -> Arc<egui::text::Galley> {
    let mut job = egui::text::LayoutJob::default();
    for (text, ty, color) in parts {
        let family = if ty.mono {
            egui::FontFamily::Monospace
        } else {
            egui::FontFamily::Proportional
        };
        let mut format = egui::text::TextFormat {
            font_id: FontId::new(ty.size, family),
            color: *color,
            extra_letter_spacing: ty.tracking,
            ..Default::default()
        };
        if ty.weight != 400.0 {
            // Variable system fonts (SF Pro, SF Mono) honour this; static fonts ignore it.
            format.coords = VariationCoords::new([(b"wght", ty.weight)]);
        }
        job.append(text, 0.0, format);
    }
    job.wrap.max_width = max_width.max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    ui.fonts_mut(|fonts| fonts.layout_job(job))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Play,
    Pause,
    Stop,
    Previous,
    Next,
    Speaker,
    Muted,
    Plus,
    More,
    Grid,
    Bars,
    Info,
    Warning,
    Equalizer,
}

pub fn configure_style(ctx: &egui::Context) -> Result<(), String> {
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.visuals = egui::Visuals::dark();
    let visuals = &mut style.visuals;
    visuals.override_text_color = Some(INK);
    visuals.panel_fill = BG;
    visuals.window_fill = RAISED;
    visuals.window_stroke = Stroke::new(1.0, RAIL);
    visuals.window_corner_radius = 12.into();
    visuals.menu_corner_radius = 8.into();
    visuals.window_shadow = egui::Shadow {
        offset: [0, 12],
        blur: 32,
        spread: 0,
        color: Color32::from_black_alpha(120),
    };
    visuals.popup_shadow = egui::Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(100),
    };
    visuals.faint_bg_color = RAISED;
    visuals.extreme_bg_color = SIDEBAR;
    visuals.text_edit_bg_color = Some(SIDEBAR);
    visuals.code_bg_color = RAISED;
    visuals.warn_fg_color = WARNING;
    visuals.error_fg_color = WARNING;
    visuals.hyperlink_color = ACCENT_INK;
    visuals.selection.bg_fill = Color32::from_rgb(0x5A, 0x3B, 0x2D);
    visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    visuals.widgets.noninteractive.bg_fill = BG;
    visuals.widgets.noninteractive.weak_bg_fill = BG;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, MUTED);
    visuals.widgets.noninteractive.corner_radius = 6.into();
    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = 6.into();
        widget.expansion = 0.0;
        widget.fg_stroke = Stroke::new(1.5, INK);
    }
    visuals.widgets.inactive.bg_fill = RAISED;
    visuals.widgets.inactive.weak_bg_fill = RAISED;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, LINE);
    visuals.widgets.hovered.bg_fill = SELECTED;
    visuals.widgets.hovered.weak_bg_fill = SELECTED;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, EDGE_HOVER);
    visuals.widgets.active.bg_fill = RAIL;
    visuals.widgets.active.weak_bg_fill = RAIL;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, EDGE_HOVER);
    visuals.widgets.open = visuals.widgets.active;
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);
    style.spacing.interact_size = Vec2::new(28.0, 28.0);
    style.spacing.window_margin = 20.into();
    style.spacing.menu_margin = 6.into();
    style.spacing.slider_rail_height = 4.0;
    style.spacing.scroll = egui::style::ScrollStyle::thin();
    style.animation_time = 0.12;
    for (text_style, size) in [
        (egui::TextStyle::Small, 11.0),
        (egui::TextStyle::Body, 13.0),
        (egui::TextStyle::Button, 13.0),
        (egui::TextStyle::Monospace, 12.5),
        (egui::TextStyle::Heading, 16.0),
    ] {
        let family = if matches!(text_style, egui::TextStyle::Monospace) {
            egui::FontFamily::Monospace
        } else {
            egui::FontFamily::Proportional
        };
        style
            .text_styles
            .insert(text_style, FontId::new(size, family));
    }
    ctx.set_style_of(egui::Theme::Dark, style);

    ctx.set_fonts(crate::system_fonts::load()?);
    Ok(())
}

/// A borderless icon button. `active` marks a toggled state (e.g. muted).
pub fn icon_button(
    ui: &mut egui::Ui,
    icon: Icon,
    tooltip: &str,
    enabled: bool,
    active: bool,
    size: f32,
) -> egui::Response {
    icon_button_reveal(ui, icon, tooltip, enabled, active, size, true)
}

/// Like [`icon_button`], but when `reveal` is false the icon is only drawn on hover or focus.
pub fn icon_button_reveal(
    ui: &mut egui::Ui,
    icon: Icon,
    tooltip: &str,
    enabled: bool,
    active: bool,
    size: f32,
    reveal: bool,
) -> egui::Response {
    ui.add_enabled(enabled, |ui: &mut egui::Ui| {
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), tooltip)
        });
        if ui.is_rect_visible(rect) {
            let enabled = ui.is_enabled();
            let hovered = enabled && (response.hovered() || response.has_focus());
            let pressed = enabled && response.is_pointer_button_down_on();
            if reveal || hovered || pressed || active {
                let fill = if active {
                    if pressed {
                        Color32::from_rgb(0x55, 0x3D, 0x32)
                    } else if hovered {
                        Color32::from_rgb(0x4A, 0x36, 0x2D)
                    } else {
                        ACCENT_TINT
                    }
                } else if pressed {
                    SELECTED
                } else if hovered {
                    RAISED
                } else {
                    Color32::TRANSPARENT
                };
                ui.painter().rect_filled(rect, 6.0, fill);
                if response.has_focus() {
                    ui.painter().rect_stroke(
                        rect.shrink(0.5),
                        6.0,
                        Stroke::new(1.5, ACCENT),
                        egui::StrokeKind::Inside,
                    );
                }
                let color = if !enabled {
                    DISABLED
                } else if active {
                    ACCENT_INK
                } else if hovered {
                    INK
                } else {
                    MUTED
                };
                let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(16.0));
                paint_icon(ui.painter(), icon_rect, icon, color);
            }
        }
        response
            .on_hover_text(tooltip)
            .on_disabled_hover_text(tooltip)
    })
}

/// The round clay play/pause button in the transport bar.
pub fn play_button(
    ui: &mut egui::Ui,
    icon: Icon,
    tooltip: &str,
    enabled: bool,
    size: f32,
) -> egui::Response {
    ui.add_enabled(enabled, |ui: &mut egui::Ui| {
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), tooltip)
        });
        if ui.is_rect_visible(rect) {
            let enabled = ui.is_enabled();
            let hovered = enabled && (response.hovered() || response.has_focus());
            let pressed = enabled && response.is_pointer_button_down_on();
            let fill = if !enabled {
                RAISED
            } else if pressed {
                ACCENT_PRESSED
            } else if hovered {
                ACCENT_HOVER
            } else {
                ACCENT
            };
            ui.painter().circle_filled(rect.center(), size / 2.0, fill);
            if response.has_focus() {
                ui.painter()
                    .circle_stroke(rect.center(), size / 2.0 + 2.0, Stroke::new(1.5, INK));
            }
            let mut icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(18.0));
            if icon == Icon::Play {
                // Optical centring: a triangle looks left-heavy when centred by its box.
                icon_rect = icon_rect.translate(Vec2::new(1.0, 0.0));
            }
            paint_icon(
                ui.painter(),
                icon_rect,
                icon,
                if enabled { SIDEBAR } else { DISABLED },
            );
        }
        response
            .on_hover_text(tooltip)
            .on_disabled_hover_text(tooltip)
    })
}

/// Icons drawn on a 16-unit grid.
pub fn paint_icon(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let s = rect.width() / 16.0;
    let pt = |x: f32, y: f32| egui::pos2(rect.left() + x * s, rect.top() + y * s);
    let stroke = Stroke::new(1.5, color);
    let line = |a: (f32, f32), b: (f32, f32)| {
        painter.line_segment([pt(a.0, a.1), pt(b.0, b.1)], stroke);
    };
    let block = |x0: f32, y0: f32, x1: f32, y1: f32, radius: f32| {
        painter.rect_filled(
            Rect::from_min_max(pt(x0, y0), pt(x1, y1)),
            radius * s,
            color,
        );
    };
    let triangle = |points: [(f32, f32); 3]| {
        painter.add(Shape::convex_polygon(
            points.into_iter().map(|(x, y)| pt(x, y)).collect(),
            color,
            Stroke::NONE,
        ));
    };
    let arc = |cx: f32, cy: f32, radius: f32, half_angle: f32| {
        let points = (0..=12)
            .map(|step| {
                let theta = -half_angle + 2.0 * half_angle * step as f32 / 12.0;
                pt(cx + radius * theta.cos(), cy + radius * theta.sin())
            })
            .collect();
        painter.add(Shape::line(points, stroke));
    };
    match icon {
        Icon::Play => triangle([(5.0, 3.2), (5.0, 12.8), (12.8, 8.0)]),
        Icon::Pause => {
            block(4.0, 3.5, 6.8, 12.5, 0.6);
            block(9.2, 3.5, 12.0, 12.5, 0.6);
        }
        Icon::Stop => block(4.0, 4.0, 12.0, 12.0, 1.2),
        Icon::Previous => {
            block(3.0, 3.5, 4.6, 12.5, 0.6);
            triangle([(12.8, 3.6), (12.8, 12.4), (5.8, 8.0)]);
        }
        Icon::Next => {
            triangle([(3.2, 3.6), (3.2, 12.4), (10.2, 8.0)]);
            block(11.4, 3.5, 13.0, 12.5, 0.6);
        }
        Icon::Speaker | Icon::Muted => {
            painter.add(Shape::closed_line(
                [
                    (2.5, 6.2),
                    (4.8, 6.2),
                    (8.0, 3.4),
                    (8.0, 12.6),
                    (4.8, 9.8),
                    (2.5, 9.8),
                ]
                .into_iter()
                .map(|(x, y)| pt(x, y))
                .collect(),
                stroke,
            ));
            if icon == Icon::Muted {
                line((10.8, 6.2), (14.2, 9.8));
                line((14.2, 6.2), (10.8, 9.8));
            } else {
                arc(8.5, 8.0, 3.0, 0.82);
                arc(8.5, 8.0, 5.6, 0.8);
            }
        }
        Icon::Plus => {
            line((8.0, 3.5), (8.0, 12.5));
            line((3.5, 8.0), (12.5, 8.0));
        }
        Icon::More => {
            for x in [3.5, 8.0, 12.5] {
                painter.circle_filled(pt(x, 8.0), 1.1 * s, color);
            }
        }
        Icon::Grid => {
            for (x, y) in [(2.5, 2.5), (9.0, 2.5), (2.5, 9.0), (9.0, 9.0)] {
                painter.rect_stroke(
                    Rect::from_min_max(pt(x, y), pt(x + 4.5, y + 4.5)),
                    1.0,
                    stroke,
                    egui::StrokeKind::Middle,
                );
            }
        }
        Icon::Bars => {
            line((3.5, 6.5), (3.5, 9.5));
            line((6.2, 4.0), (6.2, 12.0));
            line((8.9, 5.5), (8.9, 10.5));
            line((11.6, 7.0), (11.6, 9.0));
        }
        Icon::Info => {
            painter.circle_stroke(pt(8.0, 8.0), 6.0 * s, stroke);
            line((8.0, 7.3), (8.0, 11.0));
            painter.circle_filled(pt(8.0, 5.0), 0.9 * s, color);
        }
        Icon::Warning => {
            painter.add(Shape::closed_line(
                [(8.0, 2.8), (13.8, 12.8), (2.2, 12.8)]
                    .into_iter()
                    .map(|(x, y)| pt(x, y))
                    .collect(),
                stroke,
            ));
            line((8.0, 6.6), (8.0, 9.4));
            painter.circle_filled(pt(8.0, 11.0), 0.85 * s, color);
        }
        Icon::Equalizer => {
            block(3.0, 6.0, 5.0, 13.0, 1.0);
            block(7.0, 3.0, 9.0, 13.0, 1.0);
            block(11.0, 8.0, 13.0, 13.0, 1.0);
        }
    }
}

/// The LoudTrace mark on a 64-unit grid: five bars between two limiter bounds.
pub const MARK_BARS: [(f32, f32); 5] = [
    (16.0, 6.0),
    (24.0, 12.0),
    (32.0, 15.0),
    (40.0, 9.0),
    (48.0, 5.0),
];
pub const MARK_BOUNDS: [f32; 2] = [10.0, 54.0];

pub fn paint_logo(painter: &Painter, rect: Rect, bars: Color32, bounds: Color32) {
    let unit = rect.width().min(rect.height()) / 64.0;
    let origin = rect.center() - Vec2::splat(32.0 * unit);
    // Smaller sizes use slightly heavier strokes so the mark stays legible.
    let (bound_width, bar_width) = if rect.width() < 40.0 {
        (4.0, 5.0)
    } else {
        (3.5, 4.5)
    };
    let point = |x: f32, y: f32| origin + Vec2::new(x, y) * unit;
    let capsule = |a: Pos2, b: Pos2, width: f32, color: Color32| {
        let radius = width * unit / 2.0;
        painter.rect_filled(Rect::from_two_pos(a, b).expand(radius), radius, color);
    };
    for y in MARK_BOUNDS {
        capsule(point(10.0, y), point(54.0, y), bound_width, bounds);
    }
    for (x, half) in MARK_BARS {
        capsule(
            point(x, 32.0 - half),
            point(x, 32.0 + half),
            bar_width,
            bars,
        );
    }
}

/// Rasterised app icon (dock / taskbar), drawn from the same geometry as the mark.
pub fn app_icon() -> egui::IconData {
    const SIZE: u32 = 256;
    let size = SIZE as f32;
    let center = size / 2.0;
    // macOS icon grid: an 824/1024 body with a 185/1024 corner radius.
    let half = size * 824.0 / 1024.0 / 2.0;
    let radius = size * 185.0 / 1024.0;
    let mark = half * 2.0 * 0.62;
    let unit = mark / 64.0;
    let origin = center - mark / 2.0;
    let to_px = |u: f32| origin + u * unit;
    let body = [0x2B as f32, 0x27 as f32, 0x23 as f32];
    let rgb = |c: Color32| [c.r() as f32, c.g() as f32, c.b() as f32];
    type Capsule = ((f32, f32), (f32, f32), f32, [f32; 3]);
    let mut capsules: Vec<Capsule> = Vec::new();
    for y in MARK_BOUNDS {
        capsules.push((
            (to_px(10.0), to_px(y)),
            (to_px(54.0), to_px(y)),
            3.5 * unit / 2.0,
            rgb(ACCENT),
        ));
    }
    for (x, h) in MARK_BARS {
        capsules.push((
            (to_px(x), to_px(32.0 - h)),
            (to_px(x), to_px(32.0 + h)),
            4.5 * unit / 2.0,
            rgb(INK),
        ));
    }
    let rounded_rect = |px: f32, py: f32| {
        let qx = (px - center).abs() - (half - radius);
        let qy = (py - center).abs() - (half - radius);
        qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius
    };
    let segment = |p: (f32, f32), a: (f32, f32), b: (f32, f32)| {
        let (pax, pay) = (p.0 - a.0, p.1 - a.1);
        let (bax, bay) = (b.0 - a.0, b.1 - a.1);
        let h = ((pax * bax + pay * bay) / (bax * bax + bay * bay).max(1e-6)).clamp(0.0, 1.0);
        (pax - bax * h).hypot(pay - bay * h)
    };
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let p = (x as f32 + 0.5, y as f32 + 0.5);
            // Premultiplied accumulation: [r, g, b, a].
            let mut out = [0.0f32; 4];
            let mut over = |color: [f32; 3], alpha: f32| {
                let alpha = alpha.clamp(0.0, 1.0);
                for i in 0..3 {
                    out[i] = color[i] * alpha + out[i] * (1.0 - alpha);
                }
                out[3] = alpha + out[3] * (1.0 - alpha);
            };
            let shadow = rounded_rect(p.0, p.1 - size * 0.012);
            over(
                [0.0, 0.0, 0.0],
                0.28 * (1.0 - (shadow / (size * 0.035)).clamp(0.0, 1.0)),
            );
            let inside = (0.5 - rounded_rect(p.0, p.1)).clamp(0.0, 1.0);
            over(body, inside);
            for (a, b, r, color) in &capsules {
                over(
                    *color,
                    (0.5 - (segment(p, *a, *b) - r)).clamp(0.0, 1.0) * inside,
                );
            }
            let index = ((y * SIZE + x) * 4) as usize;
            if out[3] > 0.0 {
                for i in 0..3 {
                    rgba[index + i] = (out[i] / out[3]).round().clamp(0.0, 255.0) as u8;
                }
                rgba[index + 3] = (out[3] * 255.0).round() as u8;
            }
        }
    }
    egui::IconData {
        rgba,
        width: SIZE,
        height: SIZE,
    }
}

/// Style a child ui for a flat slider: 4 px rail, accent fill. Pair with [`paint_slider_handle`].
pub fn style_slider(ui: &mut egui::Ui) {
    ui.spacing_mut().interact_size.y = 15.0;
    ui.spacing_mut().slider_rail_height = 4.0;
    let visuals = ui.visuals_mut();
    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
    ] {
        widget.bg_fill = RAIL;
        widget.weak_bg_fill = RAIL;
        widget.bg_stroke = Stroke::NONE;
        widget.fg_stroke = Stroke::NONE;
        widget.expansion = 0.0;
        widget.corner_radius = 2.into();
    }
    visuals.selection.bg_fill = ACCENT;
    visuals.handle_shape = egui::style::HandleShape::Circle;
    visuals.slider_trailing_fill = true;
}

/// Draw the ink handle over a slider styled with [`style_slider`].
pub fn paint_slider_handle(ui: &egui::Ui, response: &egui::Response, fraction: f32, enabled: bool) {
    if !enabled {
        return;
    }
    let rect = response.rect;
    let radius = rect.height() / 2.5;
    let x = rect.left() + radius + (rect.width() - 2.0 * radius) * fraction.clamp(0.0, 1.0);
    let center = egui::pos2(x, rect.center().y);
    let hot = response.hovered() || response.dragged() || response.has_focus();
    let painter = ui.painter();
    painter.circle_filled(center, if hot { radius + 1.0 } else { radius }, INK);
    if response.has_focus() {
        painter.circle_stroke(center, radius + 3.0, Stroke::new(1.5, ACCENT));
    }
}
