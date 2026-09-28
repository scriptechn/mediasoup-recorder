//! Still images the composite needs (`docs/layout.md`): the avatar slate (initials on the peer's
//! signature colour, the web's `stringToColor`), name labels, the overflow name strip, the speaker border.
//! RGBA, drawn in Rust with a system TrueType font, so the composite needs no text plugin.

use crate::scene::TileStatus;
use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};
use anyhow::{Context, Result};

const TEXT: [u8; 4] = [245, 245, 247, 255];
const PILL: [u8; 4] = [18, 20, 24, 178];
const AMBER: [u8; 4] = [255, 193, 7, 255];
const RED: [u8; 4] = [255, 82, 82, 255];
const SKY: [u8; 4] = [130, 196, 255, 255];
const GREY: [u8; 4] = [205, 208, 214, 255];

/// A status icon, drawn as simple shapes so no icon font is needed.
#[derive(Clone, Copy)]
enum Icon {
    MicOff,
    Hand,
    Screen,
    Hold,
}

pub struct Image {
    pub width: u32,
    pub height: u32,
    /// RGBA, row-major, no padding.
    pub data: Vec<u8>,
}

impl Image {
    pub fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Self {
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            data.extend_from_slice(&rgba);
        }
        Self {
            width,
            height,
            data,
        }
    }

    fn blend(&mut self, x: i32, y: i32, rgba: [u8; 4], coverage: f32) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let i = ((y as u32 * self.width + x as u32) * 4) as usize;
        let a = (rgba[3] as f32 / 255.0) * coverage.clamp(0.0, 1.0);
        for (c, &v) in rgba.iter().enumerate().take(3) {
            let dst = self.data[i + c] as f32;
            self.data[i + c] = (dst + (v as f32 - dst) * a).round() as u8;
        }
        let dst_a = self.data[i + 3] as f32 / 255.0;
        self.data[i + 3] = ((a + dst_a * (1.0 - a)) * 255.0).round() as u8;
    }

    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, rgba: [u8; 4]) {
        let (x0, x1) = ((cx - r - 1.0) as i32, (cx + r + 1.0) as i32);
        let (y0, y1) = ((cy - r - 1.0) as i32, (cy + r + 1.0) as i32);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let coverage = (r - d + 0.5).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    self.blend(x, y, rgba, coverage);
                }
            }
        }
    }

    /// Anti-aliased fill of every pixel whose signed distance (negative inside) is under zero.
    fn fill_sdf(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        rgba: [u8; 4],
        sdf: impl Fn(f32, f32) -> f32,
    ) {
        for y in (y0.floor() as i32 - 1)..=(y1.ceil() as i32 + 1) {
            for x in (x0.floor() as i32 - 1)..=(x1.ceil() as i32 + 1) {
                let coverage = (0.5 - sdf(x as f32 + 0.5, y as f32 + 0.5)).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    self.blend(x, y, rgba, coverage);
                }
            }
        }
    }

    fn rounded_rect_sdf(x: f32, y: f32, w: f32, h: f32, r: f32) -> impl Fn(f32, f32) -> f32 {
        let (cx, cy, hx, hy) = (x + w / 2.0, y + h / 2.0, w / 2.0 - r, h / 2.0 - r);
        move |px, py| {
            let (qx, qy) = ((px - cx).abs() - hx, (py - cy).abs() - hy);
            (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
        }
    }

    fn fill_rounded_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, rgba: [u8; 4]) {
        let r = r.min(w / 2.0).min(h / 2.0);
        self.fill_sdf(
            x,
            y,
            x + w,
            y + h,
            rgba,
            Self::rounded_rect_sdf(x, y, w, h, r),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn stroke_rounded_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        t: f32,
        rgba: [u8; 4],
    ) {
        let r = r.min(w / 2.0).min(h / 2.0);
        let sdf = Self::rounded_rect_sdf(x, y, w, h, r);
        self.fill_sdf(x - t, y - t, x + w + t, y + h + t, rgba, move |px, py| {
            sdf(px, py).abs() - t / 2.0
        });
    }

    fn line(&mut self, ax: f32, ay: f32, bx: f32, by: f32, t: f32, rgba: [u8; 4]) {
        let (dx, dy) = (bx - ax, by - ay);
        let len2 = (dx * dx + dy * dy).max(1e-6);
        self.fill_sdf(
            ax.min(bx) - t,
            ay.min(by) - t,
            ax.max(bx) + t,
            ay.max(by) + t,
            rgba,
            move |px, py| {
                let u = (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0);
                ((px - ax - u * dx).powi(2) + (py - ay - u * dy).powi(2)).sqrt() - t / 2.0
            },
        );
    }

    /// An icon in the square at (`x`, `y`) with side `s`; shapes are laid out on a unit square.
    fn icon(&mut self, icon: Icon, x: f32, y: f32, s: f32) {
        let p = |u: f32| x + u * s;
        let q = |v: f32| y + v * s;
        match icon {
            Icon::MicOff => {
                self.fill_rounded_rect(p(0.34), q(0.04), s * 0.32, s * 0.54, s * 0.16, TEXT);
                self.line(p(0.5), q(0.64), p(0.5), q(0.82), s * 0.1, TEXT);
                self.line(p(0.3), q(0.88), p(0.7), q(0.88), s * 0.1, TEXT);
                self.line(p(0.16), q(0.1), p(0.84), q(0.9), s * 0.28, PILL);
                self.line(p(0.16), q(0.1), p(0.84), q(0.9), s * 0.13, RED);
            }
            Icon::Hand => {
                self.fill_rounded_rect(p(0.26), q(0.44), s * 0.5, s * 0.5, s * 0.16, AMBER);
                for (i, top) in [0.2, 0.06, 0.1, 0.22].iter().enumerate() {
                    let fx = 0.26 + i as f32 * 0.128;
                    self.fill_rounded_rect(
                        p(fx),
                        q(*top),
                        s * 0.115,
                        s * (0.6 - top),
                        s * 0.06,
                        AMBER,
                    );
                }
                self.line(p(0.27), q(0.62), p(0.08), q(0.44), s * 0.12, AMBER);
            }
            Icon::Screen => {
                self.stroke_rounded_rect(
                    p(0.1),
                    q(0.16),
                    s * 0.8,
                    s * 0.54,
                    s * 0.08,
                    s * 0.09,
                    SKY,
                );
                self.line(p(0.5), q(0.72), p(0.5), q(0.86), s * 0.09, SKY);
                self.line(p(0.3), q(0.9), p(0.7), q(0.9), s * 0.09, SKY);
            }
            Icon::Hold => {
                self.fill_rounded_rect(p(0.24), q(0.16), s * 0.18, s * 0.68, s * 0.06, GREY);
                self.fill_rounded_rect(p(0.58), q(0.16), s * 0.18, s * 0.68, s * 0.06, GREY);
            }
        }
    }
}

pub struct Typeface {
    font: FontVec,
    /// Material Icons; without it the status icons are drawn as shapes.
    icons: Option<FontVec>,
}

impl Typeface {
    pub fn load(path: &std::path::Path, icon_path: Option<&std::path::Path>) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("font {}", path.display()))?;
        let font = FontVec::try_from_vec(bytes).context("font is not TrueType/OpenType")?;
        let icons = icon_path.and_then(|p| match std::fs::read(p) {
            Ok(b) => FontVec::try_from_vec(b).ok(),
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e, "icon font missing; status icons drawn as shapes");
                None
            }
        });
        Ok(Self { font, icons })
    }

    /// A status icon in the square at (`x`, `y`) with side `s`: the Material Icons glyph, centred; shapes as fallback.
    fn draw_icon(&self, img: &mut Image, icon: Icon, x: f32, y: f32, s: f32) {
        if let Some(font) = &self.icons {
            let (ch, colour) = match icon {
                Icon::MicOff => ('\u{e02b}', RED),
                Icon::Hand => (
                    if font.glyph_id('\u{e764}').0 != 0 {
                        '\u{e764}'
                    } else {
                        '\u{e925}'
                    },
                    AMBER,
                ),
                Icon::Screen => ('\u{e0e2}', SKY),
                Icon::Hold => ('\u{e034}', GREY),
            };
            let id = font.glyph_id(ch);
            if id.0 != 0 {
                // Measure at the box size, then rescale so the glyph's own bounds fill 88% of the box, centred.
                let probe = id.with_scale_and_position(PxScale::from(s), point(0.0, 0.0));
                let scale = font
                    .outline_glyph(probe)
                    .map(|o| {
                        let b = o.px_bounds();
                        s * (s * 0.88 / b.width().max(b.height()).max(1.0))
                    })
                    .unwrap_or(s);
                let glyph = id.with_scale_and_position(PxScale::from(scale), point(0.0, 0.0));
                if let Some(outline) = font.outline_glyph(glyph) {
                    let b = outline.px_bounds();
                    let ox = (x + (s - b.width()) / 2.0).round() as i32;
                    let oy = (y + (s - b.height()) / 2.0).round() as i32;
                    outline.draw(|gx, gy, coverage| {
                        img.blend(ox + gx as i32, oy + gy as i32, colour, coverage);
                    });
                    return;
                }
            }
        }
        img.icon(icon, x, y, s);
    }

    /// The thumbnail: a title card in the style of a meeting-recording poster.
    #[allow(clippy::too_many_arguments)]
    pub fn title_card(
        &self,
        title: &str,
        date: &str,
        recorded_by: Option<&str>,
        participants: usize,
        brand: &str,
        width: u32,
        height: u32,
    ) -> Image {
        let mut img = Image::solid(width, height, [0, 0, 0, 255]);
        for y in 0..height {
            let k = y as f32 / height.max(1) as f32;
            let row = [
                (36.0 - 12.0 * k) as u8,
                (38.0 - 12.0 * k) as u8,
                (44.0 - 13.0 * k) as u8,
                255,
            ];
            let i = (y * width * 4) as usize;
            for x in 0..width as usize {
                img.data[i + x * 4..i + x * 4 + 4].copy_from_slice(&row);
            }
        }
        let (w, h) = (width as f32, height as f32);
        let m = w * 0.07;
        let dim: [u8; 4] = [150, 153, 162, 255];
        let soft: [u8; 4] = [214, 216, 222, 255];
        // Brand, top right.
        let bpx = h * 0.03;
        let bw = self.text_width(brand, bpx);
        self.draw(&mut img, brand, bpx, w - m - bw, m * 0.75, soft);
        // Title.
        let tpx = h * 0.078;
        let title = fit(self, title, tpx, w - 2.0 * m);
        self.draw(&mut img, &title, tpx, m, h * 0.58, TEXT);
        // When.
        self.draw(&mut img, date, h * 0.033, m, h * 0.73, soft);
        // Who and how many.
        let cpx = h * 0.02;
        let vpx = h * 0.033;
        let mut columns: Vec<(&str, String)> = Vec::new();
        if let Some(name) = recorded_by {
            columns.push(("Recorded by", name.to_string()));
        }
        let people = if participants == 1 {
            "1 participant".to_string()
        } else {
            format!("{participants} participants")
        };
        columns.push(("Participants", people));
        let mut x = m;
        for (caption, value) in columns {
            self.draw(&mut img, caption, cpx, x, h * 0.87, dim);
            self.draw(&mut img, &value, vpx, x, h * 0.93, soft);
            x += (self
                .text_width(&value, vpx)
                .max(self.text_width(caption, cpx))
                + w * 0.08)
                .max(w * 0.22);
        }
        img
    }

    fn text_width(&self, text: &str, px: f32) -> f32 {
        let scaled = self.font.as_scaled(PxScale::from(px));
        text.chars()
            .map(|c| scaled.h_advance(scaled.glyph_id(c)))
            .sum()
    }

    /// Draw `text` with its left edge at `x` and its baseline at `y`.
    fn draw(&self, img: &mut Image, text: &str, px: f32, x: f32, y: f32, rgba: [u8; 4]) {
        let scaled = self.font.as_scaled(PxScale::from(px));
        let mut caret = x;
        for c in text.chars() {
            let id = scaled.glyph_id(c);
            let glyph = id.with_scale_and_position(PxScale::from(px), point(caret, y));
            if let Some(outline) = self.font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, coverage| {
                    img.blend(
                        bounds.min.x as i32 + gx as i32,
                        bounds.min.y as i32 + gy as i32,
                        rgba,
                        coverage,
                    );
                });
            }
            caret += scaled.h_advance(id);
        }
    }

    /// The name label: a translucent pill, inset from the tile's corner, with the name and one icon per status
    /// (muted, hand raised, sharing, on hold). `height` is the image height, the pill sits inside a margin.
    pub fn label(
        &self,
        name: &str,
        status: &TileStatus,
        px: f32,
        height: u32,
        max_width: u32,
    ) -> Image {
        let margin = (height as f32 * 0.18).round();
        let pill_h = height as f32 - 2.0 * margin;
        let pad = (pill_h * 0.42).round();
        let icon = (pill_h * 0.62).round();
        let gap = (pill_h * 0.22).round();
        let mut icons = Vec::new();
        if status.sharing {
            icons.push(Icon::Screen);
        }
        if status.hold {
            icons.push(Icon::Hold);
        } else if status.muted {
            icons.push(Icon::MicOff);
        }
        if status.hand {
            icons.push(Icon::Hand);
        }
        let icons_w = icons.len() as f32 * (gap + icon);
        let text = fit(
            self,
            name,
            px,
            max_width as f32 - 2.0 * (margin + pad) - icons_w,
        );
        let pill_w = self.text_width(&text, px) + icons_w + 2.0 * pad;
        let width = ((pill_w + 2.0 * margin).ceil() as u32).clamp(1, max_width.max(1));
        let mut img = Image::solid(width, height, [0, 0, 0, 0]);
        img.fill_rounded_rect(margin, margin, pill_w, pill_h, pill_h / 2.0, PILL);
        let scaled = self.font.as_scaled(PxScale::from(px));
        let baseline = margin + (pill_h + scaled.ascent() + scaled.descent()) / 2.0;
        self.draw(&mut img, &text, px, margin + pad, baseline, TEXT);
        let mut x = margin + pad + self.text_width(&text, px) + gap;
        for i in icons {
            self.draw_icon(&mut img, i, x, margin + (pill_h - icon) / 2.0, icon);
            x += icon + gap;
        }
        img
    }

    /// Plain text in the same pill (the overflow strip).
    pub fn label_text(&self, text: &str, px: f32, height: u32, max_width: u32) -> Image {
        self.label(text, &TileStatus::default(), px, height, max_width)
    }

    /// The avatar slate: a small signature-colour disc with the initials, with a soft ring, on a dark gradient.
    pub fn slate(&self, name: &str, width: u32, height: u32, initials_px: f32) -> Image {
        let mut img = Image::solid(width, height, [24, 26, 31, 255]);
        for y in 0..height {
            let k = y as f32 / height.max(1) as f32;
            let row = [
                (32.0 - 10.0 * k) as u8,
                (34.0 - 10.0 * k) as u8,
                (40.0 - 11.0 * k) as u8,
                255,
            ];
            let i = (y * width * 4) as usize;
            for x in 0..width as usize {
                img.data[i + x * 4..i + x * 4 + 4].copy_from_slice(&row);
            }
        }
        let colour = signature_colour(name);
        let min = height.min(width) as f32;
        let radius = min * 0.17;
        let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
        img.fill_circle(
            cx,
            cy,
            radius + min * 0.03,
            [colour[0], colour[1], colour[2], 64],
        );
        img.fill_circle(cx, cy, radius, [colour[0], colour[1], colour[2], 255]);
        let text = initials(name);
        let px = initials_px.min(radius * 0.9);
        let scaled = self.font.as_scaled(PxScale::from(px));
        let w = self.text_width(&text, px);
        let baseline = cy + (scaled.ascent() + scaled.descent()) / 2.0;
        self.draw(&mut img, &text, px, cx - w / 2.0, baseline, TEXT);
        img
    }
}

/// A tile corner: an `r` by `r` black square with a transparent quarter disc, so the tile under it looks rounded.
/// `which`: 0 top-left, 1 top-right, 2 bottom-left, 3 bottom-right.
pub fn corner(r: u32, which: u32) -> Image {
    let mut img = Image::solid(r, r, [0, 0, 0, 0]);
    let rf = r as f32;
    let (cx, cy) = match which {
        0 => (rf, rf),
        1 => (0.0, rf),
        2 => (rf, 0.0),
        _ => (0.0, 0.0),
    };
    for y in 0..r {
        for x in 0..r {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let outside = (d - rf + 0.5).clamp(0.0, 1.0);
            if outside > 0.0 {
                img.blend(x as i32, y as i32, [0, 0, 0, 255], outside);
            }
        }
    }
    img
}

/// `YYYY-MM-DD HH:MM UTC` for a Unix time in milliseconds (civil-from-days, no time zone data needed).
pub fn utc_stamp(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let (hh, mm) = ((secs % 86_400) / 3600, (secs % 3600) / 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC")
}

fn fit(face: &Typeface, text: &str, px: f32, max: f32) -> String {
    if face.text_width(text, px) <= max {
        return text.to_string();
    }
    let mut s: String = text.to_string();
    while !s.is_empty() && face.text_width(&format!("{s}…"), px) > max {
        s.pop();
    }
    format!("{}…", s.trim_end())
}

pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    match words.len() {
        0 => "?".to_string(),
        1 => words[0].chars().take(2).collect::<String>().to_uppercase(),
        _ => {
            let a = words[0].chars().next().unwrap_or('?');
            let b = words[words.len() - 1].chars().next().unwrap_or('?');
            format!("{a}{b}").to_uppercase()
        }
    }
}

/// The web's `stringToColor` (`libs/shared/src/lib/helpers/function-helper.ts`): a 32-bit string hash to RGB.
pub fn signature_colour(s: &str) -> [u8; 3] {
    let mut hash: i32 = 0;
    for unit in s.encode_utf16() {
        hash = (unit as i32).wrapping_add((hash << 5).wrapping_sub(hash));
    }
    [
        (hash & 0xff) as u8,
        ((hash >> 8) & 0xff) as u8,
        ((hash >> 16) & 0xff) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_matches_the_web_helper() {
        // node: stringToColor('Alice') === '#60a6c6', stringToColor('E2E A') === '#7917e4'
        assert_eq!(signature_colour("Alice"), [0x60, 0xa6, 0xc6]);
        assert_eq!(signature_colour("E2E A"), [0x79, 0x17, 0xe4]);
    }

    #[test]
    fn utc_stamp_formats_civil_time() {
        assert_eq!(utc_stamp(1_699_448_880_000), "2023-11-08 13:08 UTC");
        assert_eq!(utc_stamp(0), "1970-01-01 00:00 UTC");
    }

    #[test]
    fn initials_take_first_and_last_word() {
        assert_eq!(initials("Alice Smith"), "AS");
        assert_eq!(initials("bob"), "BO");
        assert_eq!(initials(""), "?");
    }
}
