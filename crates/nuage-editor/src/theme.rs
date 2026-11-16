//! The console's colors (the CSS tokens of its pages) as the window's, in
//! light and dark; the text sizes; and the marks drawn next to a word (a
//! check, a cross, a dot), drawn rather than typed: a font may lack them.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Vec2};

/// The colors of one theme, named as the console's CSS names them.
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
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// The console's light theme.
pub const LIGHT: Palette = Palette {
    bg: hex(0xf5f7fc),
    bg2: hex(0xeef1f8),
    card: hex(0xffffff),
    fg: hex(0x0f1426),
    muted: hex(0x4d5876),
    line: hex(0xdde2ee),
    line2: hex(0xc9d0e2),
    accent: hex(0x0369a1),
    button: hex(0x0b6fb8),
    on_button: hex(0xffffff),
    ok: hex(0x047857),
    warn: hex(0xb45309),
    error: hex(0xb91c1c),
    code: hex(0xf2f5fb),
};

/// The console's dark theme.
pub const DARK: Palette = Palette {
    bg: hex(0x060913),
    bg2: hex(0x0a0f1e),
    card: hex(0x0e1427),
    fg: hex(0xe9edf8),
    muted: hex(0x9aa4c2),
    line: hex(0x1d2742),
    line2: hex(0x2b3858),
    accent: hex(0x7dd3fc),
    button: hex(0x38bdf8),
    on_button: hex(0x04121f),
    ok: hex(0x34d399),
    warn: hex(0xfbbf24),
    error: hex(0xf87171),
    code: hex(0x0a1020),
};

/// The palette of the theme `ui` is drawn in.
pub fn of(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode { &DARK } else { &LIGHT }
}

/// The window's visuals in the colors of `p`.
pub fn visuals(p: &Palette, dark: bool) -> egui::Visuals {
    let mut v = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    let radius = CornerRadius::same(8);
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.window_stroke = Stroke::new(1.0, p.line);
    v.window_corner_radius = CornerRadius::same(10);
    v.extreme_bg_color = p.bg2;
    v.text_edit_bg_color = Some(p.bg2);
    v.faint_bg_color = p.bg2;
    v.code_bg_color = p.code;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.error;
    v.weak_text_color = Some(p.muted);
    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, p.fg);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.card;
    w.noninteractive.weak_bg_fill = p.card;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.fg);
    w.noninteractive.corner_radius = radius;
    for (state, stroke, width) in [
        (&mut w.inactive, p.line2, 1.0),
        (&mut w.hovered, p.accent, 1.5),
        (&mut w.active, p.accent, 2.0),
        (&mut w.open, p.accent, 1.0),
    ] {
        state.bg_fill = p.bg2;
        state.weak_bg_fill = p.card;
        state.bg_stroke = Stroke::new(1.0, stroke);
        state.fg_stroke = Stroke::new(width, p.fg);
        state.corner_radius = radius;
        state.expansion = 0.0;
    }
    v
}

/// Text sizes: body 15, headings 22 and 18, buttons at least 32 high.
pub fn style(style: &mut egui::Style) {
    style.text_styles = [
        (TextStyle::Small, FontId::new(12.5, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(15.0, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(22.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(14.0, FontFamily::Monospace)),
    ]
    .into();
    style.spacing.button_padding = Vec2::new(12.0, 7.0);
    style.spacing.item_spacing = Vec2::new(8.0, 7.0);
}

/// Puts the console's colors and sizes in both themes of `ctx`. Where the
/// system does not say its theme (often on Linux), the window is light, as
/// the site is by default.
pub fn install(ctx: &egui::Context) {
    ctx.set_visuals_of(egui::Theme::Light, visuals(&LIGHT, false));
    ctx.set_visuals_of(egui::Theme::Dark, visuals(&DARK, true));
    ctx.all_styles_mut(style);
    ctx.options_mut(|o| o.fallback_theme = egui::Theme::Light);
}

/// A section's title (18 px: a size, not a style of its own, which the
/// first frame would not have yet).
pub fn subheading(ui: &mut egui::Ui, text: impl Into<String>) -> egui::Response {
    ui.label(egui::RichText::new(text.into()).size(18.0).strong())
}

/// A button for the page's main action, in the console's button color.
pub fn primary(text: impl Into<String>, ui: &egui::Ui) -> egui::Button<'static> {
    let p = of(ui);
    egui::Button::new(egui::RichText::new(text.into()).color(p.on_button).strong()).fill(p.button)
}

/// A button that cannot be undone (revoke), in the console's danger color.
pub fn danger(text: impl Into<String>, ui: &egui::Ui) -> egui::Button<'static> {
    let p = of(ui);
    egui::Button::new(egui::RichText::new(text.into()).color(p.card).strong()).fill(p.error)
}

/// A card, as the console draws them.
pub fn card(ui: &egui::Ui) -> egui::Frame {
    let p = of(ui);
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.line))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(14))
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
    let p = of(ui);
    let size = ui.text_style_height(&TextStyle::Body).max(14.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    let c = rect.center();
    let r = size * 0.32;
    let color = mark.color(p);
    let painter = ui.painter();
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
    response
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
