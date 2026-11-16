//! The console's colors (the CSS tokens of its pages) as the window's, in
//! light and dark; the text sizes; and the marks drawn next to a word (a
//! check, a cross, a dot), drawn rather than typed: a font may lack them.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Vec2};

/// The colors of one theme: the console's, its accent the indigo of the
/// site's gradient.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: Color32,
    pub bg2: Color32,
    pub card: Color32,
    pub fg: Color32,
    pub muted: Color32,
    pub line: Color32,
    pub line2: Color32,
    pub accent: Color32,
    /// A button that does the page's main thing, and its text.
    pub button: Color32,
    pub on_button: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub error: Color32,
    pub code: Color32,
    /// Under the pointer: a section of the side bar, a row.
    pub hover: Color32,
    /// The section shown, in the side bar; a tinted tile.
    pub selected: Color32,
    /// The side bar and the top bar.
    pub chrome: Color32,
    /// The shadow of cards and windows.
    pub shadow: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// The light theme.
pub const LIGHT: Palette = Palette {
    bg: hex(0xf4f6fb),
    bg2: hex(0xf1f3f9),
    card: hex(0xffffff),
    fg: hex(0x0f172a),
    muted: hex(0x56607a),
    line: hex(0xe4e8f1),
    line2: hex(0xcfd6e4),
    accent: hex(0x4f46e5),
    button: hex(0x4f46e5),
    on_button: hex(0xffffff),
    ok: hex(0x047857),
    warn: hex(0xb45309),
    error: hex(0xb91c1c),
    code: hex(0xf3f5fa),
    hover: hex(0xeef1f8),
    selected: hex(0xebeafe),
    chrome: hex(0xfbfcfe),
    shadow: Color32::from_rgba_premultiplied(15, 23, 42, 18),
};

/// The dark theme.
pub const DARK: Palette = Palette {
    bg: hex(0x0a0e1a),
    bg2: hex(0x0f1527),
    card: hex(0x111829),
    fg: hex(0xe7ebf6),
    muted: hex(0x9aa5c2),
    line: hex(0x1e2740),
    line2: hex(0x2c3858),
    accent: hex(0xa5b4fc),
    button: hex(0x818cf8),
    on_button: hex(0x0a0e1a),
    ok: hex(0x34d399),
    warn: hex(0xfbbf24),
    error: hex(0xf87171),
    code: hex(0x0c1222),
    hover: hex(0x172038),
    selected: hex(0x1f2550),
    chrome: hex(0x0c1120),
    shadow: Color32::from_rgba_premultiplied(0, 0, 0, 90),
};

/// The palette of the theme `ui` is drawn in.
pub fn of(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

/// The window's visuals in the colors of `p`.
pub fn visuals(p: &Palette, dark: bool) -> egui::Visuals {
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    let radius = CornerRadius::same(10);
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.window_stroke = Stroke::new(1.0, p.line);
    v.window_corner_radius = CornerRadius::same(16);
    v.window_shadow = egui::epaint::Shadow { offset: [0, 12], blur: 32, spread: 0, color: p.shadow };
    v.popup_shadow = egui::epaint::Shadow { offset: [0, 6], blur: 18, spread: 0, color: p.shadow };
    v.menu_corner_radius = CornerRadius::same(10);
    v.extreme_bg_color = p.bg2;
    v.text_edit_bg_color = Some(p.bg2);
    v.faint_bg_color = p.bg2;
    v.code_bg_color = p.code;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.error;
    v.weak_text_color = Some(p.muted);
    v.selection.bg_fill = p.accent.gamma_multiply(0.30);
    v.selection.stroke = Stroke::new(1.0, p.fg);
    v.indent_has_left_vline = false;
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.card;
    w.noninteractive.weak_bg_fill = p.card;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.fg);
    w.noninteractive.corner_radius = radius;
    // Buttons: white (or a card's color) with a fine border; fields: a soft fill.
    for (state, fill, stroke, width) in [
        (&mut w.inactive, p.card, p.line2, 1.0),
        (&mut w.hovered, p.hover, p.accent.gamma_multiply(0.7), 1.0),
        (&mut w.active, p.selected, p.accent, 1.5),
        (&mut w.open, p.hover, p.accent, 1.0),
    ] {
        state.bg_fill = p.bg2;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::new(width, stroke);
        state.fg_stroke = Stroke::new(1.0, p.fg);
        state.corner_radius = radius;
        state.expansion = 0.0;
    }
    v
}

/// The fonts of the site: Inter for the words, Space Grotesk for the titles,
/// JetBrains Mono for tokens and addresses (all under the SIL Open Font
/// License, in `assets/fonts`). egui's own stay behind them, for the
/// characters they lack.
pub fn fonts() -> egui::FontDefinitions {
    use std::sync::Arc;
    let mut fonts = egui::FontDefinitions::default();
    for (name, data) in [
        ("inter", &include_bytes!("../assets/fonts/Inter-Regular.ttf")[..]),
        ("inter-semibold", &include_bytes!("../assets/fonts/Inter-SemiBold.ttf")[..]),
        ("space-grotesk", &include_bytes!("../assets/fonts/SpaceGrotesk-SemiBold.ttf")[..]),
        ("jetbrains-mono", &include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf")[..]),
    ] {
        fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_static(data)));
    }
    let proportional = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let monospace = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let with = |first: &[&str], rest: &[String]| -> Vec<String> {
        first.iter().map(|s| (*s).to_owned()).chain(rest.iter().cloned()).collect()
    };
    fonts.families.insert(FontFamily::Proportional, with(&["inter"], &proportional));
    fonts.families.insert(FontFamily::Monospace, with(&["jetbrains-mono"], &monospace));
    fonts.families.insert(FontFamily::Name(STRONG.into()), with(&["inter-semibold"], &proportional));
    fonts.families.insert(FontFamily::Name(TITLE.into()), with(&["space-grotesk", "inter-semibold"], &proportional));
    fonts
}

/// The family of words that stand out (buttons, titles of cards).
pub const STRONG: &str = "strong";
/// The family of the titles of pages.
pub const TITLE: &str = "title";

fn strong_family() -> FontFamily {
    FontFamily::Name(STRONG.into())
}

/// Text sizes: body 14.5, page titles 26, buttons and fields 34 high.
pub fn style(style: &mut egui::Style) {
    style.text_styles = [
        (TextStyle::Small, FontId::new(12.5, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(14.5, strong_family())),
        (TextStyle::Heading, FontId::new(26.0, FontFamily::Name(TITLE.into()))),
        (TextStyle::Monospace, FontId::new(13.5, FontFamily::Monospace)),
    ]
    .into();
    style.spacing.button_padding = Vec2::new(14.0, 8.0);
    style.spacing.item_spacing = Vec2::new(10.0, 8.0);
    style.spacing.interact_size = Vec2::new(40.0, 34.0);
    style.spacing.menu_margin = egui::Margin::same(8);
    style.spacing.window_margin = egui::Margin::same(20);
    style.spacing.combo_width = 120.0;
    style.spacing.scroll = egui::style::ScrollStyle::floating();
}

/// Puts the colors, the fonts and the sizes in both themes of `ctx`. Where
/// the system does not say its theme (often on Linux), the window is light,
/// as the site is by default.
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_visuals_of(egui::Theme::Light, visuals(&LIGHT, false));
    ctx.set_visuals_of(egui::Theme::Dark, visuals(&DARK, true));
    ctx.all_styles_mut(style);
    ctx.options_mut(|o| o.fallback_theme = egui::Theme::Light);
}

/// The title of a page (Space Grotesk), with a line of what it is for under it.
pub fn page_title(ui: &mut egui::Ui, title: impl Into<String>, about: Option<&str>) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(title.into()).text_style(TextStyle::Heading).color(of(ui).fg));
    if let Some(about) = about {
        ui.label(egui::RichText::new(about).color(of(ui).muted));
    }
    ui.add_space(10.0);
}

/// A section's title in a card (17, semibold: a size, not a style of its
/// own, which the first frame would not have yet).
pub fn subheading(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    ui.label(egui::RichText::new(text.into()).size(17.0).family(strong_family()).color(of(ui).fg))
}

/// Words that stand out within a line.
pub fn strong(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into()).family(strong_family())
}

/// A field's name, in the strong family.
pub fn strong_label(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    let color = of(ui).fg;
    ui.label(strong(text).color(color))
}

/// A button for the page's main action, in the accent color.
pub fn primary(text: impl Into<String>, ui: &egui::Ui) -> egui::Button<'static> {
    let p = of(ui);
    egui::Button::new(egui::RichText::new(text.into()).color(p.on_button).family(strong_family()))
        .fill(p.button)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(10))
        .min_size(Vec2::new(0.0, 36.0))
}

/// A button that cannot be undone (revoke), in the danger color.
pub fn danger(text: impl Into<String>, ui: &egui::Ui) -> egui::Button<'static> {
    let p = of(ui);
    egui::Button::new(egui::RichText::new(text.into()).color(p.card).family(strong_family()))
        .fill(p.error)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(10))
        .min_size(Vec2::new(0.0, 36.0))
}

/// A card: white (or deep blue), rounded, with a soft shadow.
pub fn card(ui: &egui::Ui) -> egui::Frame {
    let p = of(ui);
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.line))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(egui::Margin::same(20))
        .shadow(egui::epaint::Shadow { offset: [0, 2], blur: 10, spread: 0, color: p.shadow })
}

/// A small rounded label with a mark: "Online", "Encrypted", "Post-quantum".
pub fn pill(ui: &mut egui::Ui, m: Mark, text: impl Into<String>) -> egui::Response {
    // One piece of a known size: in a wrapped row, it goes to the next line whole.
    let p = of(ui);
    let color = m.color(p);
    let text = text.into();
    let galley = ui.painter().layout_no_wrap(text.clone(), egui::FontId::new(13.0, strong_family()), color);
    let dot = 14.0;
    let size = Vec2::new(10.0 + dot + 5.0 + galley.size().x + 10.0, (galley.size().y + 8.0).max(24.0));
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(255), color.gamma_multiply(0.12));
        let mark_rect =
            egui::Rect::from_center_size(egui::pos2(rect.left() + 10.0 + dot / 2.0, rect.center().y), Vec2::splat(dot));
        paint_mark(painter, mark_rect, m, color);
        let at = egui::pos2(mark_rect.right() + 5.0, rect.center().y - galley.size().y / 2.0);
        painter.galley(at, galley, color);
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, &text));
    response
}

/// A figure in a tile: its icon, what it is, the figure large, a line under it.
pub fn tile(ui: &mut egui::Ui, icon: Icon, label: &str, value: &str, under: Option<(Mark, String)>) {
    let p = of(ui);
    card(ui).inner_margin(egui::Margin::same(16)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            icon_badge(ui, icon);
            ui.label(egui::RichText::new(label).color(p.muted).size(13.0));
        });
        ui.add_space(4.0);
        ui.label(egui::RichText::new(value).size(22.0).family(FontFamily::Name(TITLE.into())).color(p.fg));
        if let Some((m, text)) = under {
            ui.label(egui::RichText::new(text).color(m.color(p)).size(12.5));
        }
    });
}

/// A gauge: `fraction` of a rounded bar, in the mark's color past 85 %.
pub fn meter(ui: &mut egui::Ui, fraction: f32) {
    let p = of(ui);
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 8.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(4), p.line);
    let f = fraction.clamp(0.0, 1.0);
    let color = if f >= 0.85 { p.warn } else { p.accent };
    if f > 0.0 {
        let filled = egui::Rect::from_min_size(rect.min, Vec2::new((width * f).max(8.0), rect.height()));
        painter.rect_filled(filled, CornerRadius::same(4), color);
    }
}

/// The icons of the window, drawn with lines (a font may lack them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Overview,
    Applications,
    Memory,
    Limits,
    Startup,
    Computer,
    Tools,
    About,
    Console,
    Dashboard,
    Refresh,
    Backup,
    Lock,
    Clock,
    Server,
}

/// Draws `icon` in `rect`, in `color`.
pub fn paint_icon(painter: &egui::Painter, rect: egui::Rect, icon: Icon, color: Color32) {
    use egui::{Pos2, pos2};
    let s = Stroke::new(1.6, color);
    let r = rect.shrink(rect.width() * 0.14);
    let at = |x: f32, y: f32| -> Pos2 { pos2(r.left() + r.width() * x, r.top() + r.height() * y) };
    let rr = |x0: f32, y0: f32, x1: f32, y1: f32| egui::Rect::from_two_pos(at(x0, y0), at(x1, y1));
    let round = CornerRadius::same(2);
    let outside = egui::StrokeKind::Inside;
    match icon {
        Icon::Overview => {
            for (x, y) in [(0.0, 0.0), (0.56, 0.0), (0.0, 0.56), (0.56, 0.56)] {
                painter.rect_stroke(rr(x, y, x + 0.44, y + 0.44), round, s, outside);
            }
        }
        Icon::Applications => {
            painter.rect_stroke(rr(0.0, 0.05, 1.0, 0.95), round, s, outside);
            painter.line_segment([at(0.0, 0.35), at(1.0, 0.35)], s);
            for x in [0.14, 0.27, 0.40] {
                painter.circle_filled(at(x, 0.2), 1.1, color);
            }
        }
        Icon::Memory => {
            painter.rect_stroke(rr(0.15, 0.15, 0.85, 0.85), round, s, outside);
            painter.rect_stroke(rr(0.35, 0.35, 0.65, 0.65), CornerRadius::same(1), s, outside);
            for i in [0.35, 0.5, 0.65] {
                painter.line_segment([at(i, 0.0), at(i, 0.15)], s);
                painter.line_segment([at(i, 0.85), at(i, 1.0)], s);
                painter.line_segment([at(0.0, i), at(0.15, i)], s);
                painter.line_segment([at(0.85, i), at(1.0, i)], s);
            }
        }
        Icon::Limits => {
            for (y, knob) in [(0.2, 0.3), (0.5, 0.7), (0.8, 0.45)] {
                painter.line_segment([at(0.0, y), at(1.0, y)], s);
                painter.circle_filled(at(knob, y), r.width() * 0.1, color);
            }
        }
        Icon::Startup => {
            painter.line_segment([at(0.5, 0.0), at(0.5, 0.45)], s);
            let c = at(0.5, 0.55);
            let radius = r.width() * 0.42;
            let points: Vec<Pos2> = (0..=24)
                .map(|i| {
                    let a = -std::f32::consts::FRAC_PI_2 + 0.75 + (i as f32 / 24.0) * (std::f32::consts::TAU - 1.5);
                    c + Vec2::new(a.cos(), a.sin()) * radius
                })
                .collect();
            painter.add(egui::Shape::line(points, s));
        }
        Icon::Computer => {
            painter.rect_stroke(rr(0.0, 0.05, 1.0, 0.7), round, s, outside);
            painter.line_segment([at(0.5, 0.7), at(0.5, 0.9)], s);
            painter.line_segment([at(0.25, 0.95), at(0.75, 0.95)], s);
        }
        Icon::Tools => {
            painter.line_segment([at(0.15, 0.85), at(0.6, 0.4)], Stroke::new(2.4, color));
            painter.circle_stroke(at(0.7, 0.3), r.width() * 0.22, s);
        }
        Icon::About => {
            painter.circle_stroke(r.center(), r.width() * 0.48, s);
            painter.line_segment([at(0.5, 0.45), at(0.5, 0.75)], s);
            painter.circle_filled(at(0.5, 0.28), 1.3, color);
        }
        Icon::Console => {
            painter.rect_stroke(rr(0.0, 0.1, 1.0, 0.9), round, s, outside);
            painter.line_segment([at(0.18, 0.35), at(0.35, 0.5)], s);
            painter.line_segment([at(0.35, 0.5), at(0.18, 0.65)], s);
            painter.line_segment([at(0.45, 0.68), at(0.75, 0.68)], s);
        }
        Icon::Dashboard => {
            for (x, h) in [(0.12, 0.45), (0.42, 0.75), (0.72, 0.3)] {
                painter.rect_stroke(rr(x, 1.0 - h, x + 0.18, 1.0), CornerRadius::same(1), s, outside);
            }
        }
        Icon::Refresh => {
            let c = r.center();
            let radius = r.width() * 0.45;
            let points: Vec<Pos2> = (0..=24)
                .map(|i| {
                    let a = 0.4 + (i as f32 / 24.0) * (std::f32::consts::TAU - 1.2);
                    c + Vec2::new(a.cos(), a.sin()) * radius
                })
                .collect();
            let end = *points.last().unwrap_or(&c);
            painter.add(egui::Shape::line(points, s));
            painter.line_segment([end, end + Vec2::new(r.width() * 0.25, -r.width() * 0.02)], s);
            painter.line_segment([end, end + Vec2::new(0.0, r.width() * 0.25)], s);
        }
        Icon::Backup => {
            painter.rect_stroke(rr(0.0, 0.0, 1.0, 0.3), round, s, outside);
            painter.rect_stroke(rr(0.0, 0.35, 1.0, 0.65), round, s, outside);
            painter.rect_stroke(rr(0.0, 0.7, 1.0, 1.0), round, s, outside);
        }
        Icon::Lock => {
            painter.rect_stroke(rr(0.1, 0.45, 0.9, 1.0), round, s, outside);
            let c = at(0.5, 0.45);
            let radius = r.width() * 0.27;
            let points: Vec<Pos2> = (0..=16)
                .map(|i| {
                    let a = std::f32::consts::PI + (i as f32 / 16.0) * std::f32::consts::PI;
                    c + Vec2::new(a.cos() * radius, a.sin() * radius * 1.3)
                })
                .collect();
            painter.add(egui::Shape::line(points, s));
        }
        Icon::Clock => {
            painter.circle_stroke(r.center(), r.width() * 0.48, s);
            painter.line_segment([r.center(), at(0.5, 0.2)], s);
            painter.line_segment([r.center(), at(0.72, 0.6)], s);
        }
        Icon::Server => {
            painter.rect_stroke(rr(0.0, 0.05, 1.0, 0.45), round, s, outside);
            painter.rect_stroke(rr(0.0, 0.55, 1.0, 0.95), round, s, outside);
            painter.circle_filled(at(0.2, 0.25), 1.3, color);
            painter.circle_filled(at(0.2, 0.75), 1.3, color);
        }
    }
}

/// An icon in a small rounded square of the accent's tint.
pub fn icon_badge(ui: &mut egui::Ui, icon: Icon) {
    let p = of(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(26.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(8), p.selected);
    paint_icon(ui.painter(), rect.shrink(5.0), icon, p.accent);
}

/// A section of the side bar: an icon and its name, the one shown on a
/// tinted background with a bar of the accent. A button for screen readers
/// and the tests, named by its words.
pub fn nav_item(ui: &mut egui::Ui, icon: Icon, label: &str, selected: bool) -> egui::Response {
    let p = of(ui);
    let height = 36.0;
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, ui.is_enabled(), selected, label));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let fill = if selected {
            p.selected
        } else if response.hovered() {
            p.hover
        } else {
            Color32::TRANSPARENT
        };
        painter.rect_filled(rect, CornerRadius::same(10), fill);
        if selected {
            let bar = egui::Rect::from_min_size(rect.left_top() + Vec2::new(0.0, 9.0), Vec2::new(3.0, height - 18.0));
            painter.rect_filled(bar, CornerRadius::same(2), p.accent);
        }
        let color = if selected { p.accent } else { p.muted };
        let icon_rect = egui::Rect::from_center_size(rect.left_center() + Vec2::new(22.0, 0.0), Vec2::splat(18.0));
        paint_icon(painter, icon_rect, icon, color);
        let text_color = if selected { p.fg } else { p.fg.gamma_multiply(0.85) };
        let font = if selected { FontId::new(14.5, strong_family()) } else { FontId::proportional(14.5) };
        let galley = painter.layout(label.to_owned(), font, text_color, width - 52.0);
        painter.galley(rect.left_center() + Vec2::new(42.0, -galley.size().y / 2.0), galley, text_color);
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// The mark of Nuage DB: a cloud in the colors of the site's gradient.
pub fn logo(ui: &mut egui::Ui, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(size * 1.4, size), egui::Sense::hover());
    let painter = ui.painter();
    let w = rect.width();
    let base = rect.bottom() - size * 0.18;
    let blue = hex(0x38bdf8);
    let indigo = hex(0x6366f1);
    let purple = hex(0xa855f7);
    painter.circle_filled(egui::pos2(rect.left() + w * 0.30, base - size * 0.22), size * 0.24, blue);
    painter.circle_filled(egui::pos2(rect.left() + w * 0.72, base - size * 0.22), size * 0.22, purple);
    painter.circle_filled(egui::pos2(rect.left() + w * 0.50, base - size * 0.38), size * 0.33, indigo);
    let body = egui::Rect::from_min_max(
        egui::pos2(rect.left() + w * 0.30, base - size * 0.34),
        egui::pos2(rect.left() + w * 0.72, base),
    );
    painter.rect_filled(body, CornerRadius::same(2), indigo);
    painter.circle_filled(
        egui::pos2(rect.left() + w * 0.30, base - size * 0.22),
        size * 0.22,
        blue.lerp_to_gamma(indigo, 0.35),
    );
    painter.circle_filled(
        egui::pos2(rect.left() + w * 0.72, base - size * 0.20),
        size * 0.20,
        purple.lerp_to_gamma(indigo, 0.35),
    );
    response
}

/// What a mark says, always with a word next to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// It works (a check).
    Ok,
    /// Something to look at (an amber dot).
    Warn,
    /// It fails (a cross).
    Bad,
    /// Connected, running (a filled dot).
    On,
    /// Not connected, not running (a ring).
    Off,
}

impl Mark {
    /// The color the words next to it take.
    pub fn color(self, p: &Palette) -> Color32 {
        match self {
            Mark::Ok | Mark::On => p.ok,
            Mark::Warn => p.warn,
            Mark::Bad => p.error,
            Mark::Off => p.muted,
        }
    }
}

/// Draws `mark` in a square the height of a line.
pub fn mark(ui: &mut egui::Ui, mark: Mark) -> egui::Response {
    let size = ui.text_style_height(&TextStyle::Body).max(14.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    paint_mark(ui.painter(), rect, mark, mark.color(of(ui)));
    response
}

/// Draws a mark in a square.
fn paint_mark(painter: &egui::Painter, rect: egui::Rect, mark: Mark, color: Color32) {
    let c = rect.center();
    let r = rect.width() * 0.32;
    match mark {
        Mark::Ok => {
            let stroke = Stroke::new(2.0, color);
            painter.line_segment([c + Vec2::new(-r, 0.0), c + Vec2::new(-r * 0.3, r * 0.7)], stroke);
            painter.line_segment([c + Vec2::new(-r * 0.3, r * 0.7), c + Vec2::new(r, -r * 0.8)], stroke);
        }
        Mark::Bad => {
            let stroke = Stroke::new(2.0, color);
            painter.line_segment([c + Vec2::new(-r * 0.8, -r * 0.8), c + Vec2::new(r * 0.8, r * 0.8)], stroke);
            painter.line_segment([c + Vec2::new(-r * 0.8, r * 0.8), c + Vec2::new(r * 0.8, -r * 0.8)], stroke);
        }
        Mark::Warn | Mark::On => {
            painter.circle_filled(c, r * 0.85, color);
        }
        Mark::Off => {
            painter.circle_stroke(c, r * 0.8, Stroke::new(1.5, color));
        }
    }
}

/// A mark and its words, in the mark's color (never a color alone).
pub fn status(ui: &mut egui::Ui, m: Mark, text: impl Into<String>) {
    let color = m.color(of(ui));
    ui.horizontal_wrapped(|ui| {
        mark(ui, m);
        ui.label(egui::RichText::new(text.into()).color(color));
    });
}

/// The WCAG contrast ratio of two colors (1 to 21).
pub fn contrast(a: Color32, b: Color32) -> f32 {
    fn luminance(c: Color32) -> f32 {
        let channel = |v: u8| {
            let v = f32::from(v) / 255.0;
            if v <= 0.040_45 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
    }
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_text_color_reads_on_every_background() {
        for (name, p) in [("light", LIGHT), ("dark", DARK)] {
            for (what, fg) in [("text", p.fg), ("muted", p.muted), ("ok", p.ok), ("warn", p.warn), ("error", p.error)] {
                for (on, bg) in [("bg", p.bg), ("card", p.card)] {
                    let ratio = contrast(fg, bg);
                    assert!(ratio >= 4.5, "{name}: {what} on {on} is {ratio:.2}");
                }
            }
            assert!(contrast(p.on_button, p.button) >= 4.5, "{name}: button text");
            assert!(contrast(p.card, p.error) >= 4.5, "{name}: danger button text");
            assert!(contrast(p.accent, p.card) >= 4.5, "{name}: links");
        }
        assert!((contrast(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
    }
}
