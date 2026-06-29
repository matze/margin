//! Render a drawn ratatui [`Buffer`] to a standalone SVG, used to generate the
//! README screenshots. Pure and terminal-free: the output is a deterministic
//! function of the buffer and theme, with the 16 ANSI slots and the default
//! fg/bg pinned to a fixed scheme so the result never depends on the reader's
//! terminal.

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use super::theme::ThemeMode;

/// Pixel size of one terminal cell in the emitted SVG.
const CELL_W: usize = 8;
const CELL_H: usize = 17;
/// Glyph baseline offset within a cell row.
const BASELINE: usize = 13;
const FONT_SIZE: usize = 13;
const FONT_FAMILY: &str =
    "ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, 'Liberation Mono', monospace";

/// A fixed terminal color scheme: the canvas default fg/bg plus the 16 ANSI
/// slots, so ANSI-named styles render to concrete RGB independent of the
/// reader's terminal.
struct Scheme {
    fg: &'static str,
    bg: &'static str,
    ansi: [&'static str; 16],
}

impl Scheme {
    fn for_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Scheme {
                fg: "#c9d1d9",
                bg: "#0d1117",
                ansi: [
                    "#484f58", "#ff7b72", "#3fb950", "#d29922", "#58a6ff", "#bc8cff", "#39c5cf",
                    "#b1bac4", "#6e7681", "#ffa198", "#56d364", "#e3b341", "#79c0ff", "#d2a8ff",
                    "#56d4dd", "#f0f6fc",
                ],
            },
            ThemeMode::Light => Scheme {
                fg: "#1f2328",
                bg: "#ffffff",
                ansi: [
                    "#24292f", "#cf222e", "#1a7f37", "#9a6700", "#0969da", "#8250df", "#1b7c83",
                    "#6e7781", "#6e7781", "#a40e26", "#1a7f37", "#7d4e00", "#0550ae", "#6639ba",
                    "#1b7c83", "#8c959f",
                ],
            },
        }
    }

    /// Resolve a color to a concrete `#rrggbb`, or `None` for the terminal
    /// default (`Color::Reset`) which the caller maps to the canvas fg/bg.
    fn resolve(&self, color: Color) -> Option<String> {
        let ansi = |index: usize| Some(self.ansi[index].to_string());

        match color {
            Color::Reset => None,
            Color::Black => ansi(0),
            Color::Red => ansi(1),
            Color::Green => ansi(2),
            Color::Yellow => ansi(3),
            Color::Blue => ansi(4),
            Color::Magenta => ansi(5),
            Color::Cyan => ansi(6),
            Color::Gray => ansi(7),
            Color::DarkGray => ansi(8),
            Color::LightRed => ansi(9),
            Color::LightGreen => ansi(10),
            Color::LightYellow => ansi(11),
            Color::LightBlue => ansi(12),
            Color::LightMagenta => ansi(13),
            Color::LightCyan => ansi(14),
            Color::White => ansi(15),
            Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
            Color::Indexed(index) => Some(self.indexed(index)),
        }
    }

    /// Map an xterm-256 palette index to RGB: the first 16 reuse the ANSI slots,
    /// 16..=231 form the 6×6×6 color cube, and the rest are the grayscale ramp.
    fn indexed(&self, index: u8) -> String {
        match index {
            0..=15 => self.ansi[index as usize].to_string(),
            16..=231 => {
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                let i = index - 16;
                let (r, g, b) = (level(i / 36), level((i / 6) % 6), level(i % 6));
                format!("#{r:02x}{g:02x}{b:02x}")
            }
            _ => {
                let v = 8 + (index - 232) * 10;
                format!("#{v:02x}{v:02x}{v:02x}")
            }
        }
    }
}

/// Render `buffer` to a self-contained SVG string.
pub fn buffer_to_svg(buffer: &Buffer, mode: ThemeMode) -> String {
    let scheme = Scheme::for_mode(mode);
    let area = buffer.area;
    let cols = area.width as usize;
    let rows = area.height as usize;
    let (px_w, px_h) = (cols * CELL_W, rows * CELL_H);

    let cell = |x: usize, y: usize| buffer.cell((area.x + x as u16, area.y + y as u16));

    let mut out = String::new();
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{px_w}\" height=\"{px_h}\" \
viewBox=\"0 0 {px_w} {px_h}\" font-family=\"{FONT_FAMILY}\" font-size=\"{FONT_SIZE}px\">\n"
    ));
    out.push_str(&format!(
        "<rect width=\"{px_w}\" height=\"{px_h}\" fill=\"{}\"/>\n",
        scheme.bg
    ));

    for y in 0..rows {
        let mut x = 0;

        while x < cols {
            let fill = cell(x, y).and_then(|c| scheme.resolve(c.bg));

            if fill.is_none() {
                x += 1;
                continue;
            }

            let start = x;

            while x < cols && cell(x, y).and_then(|c| scheme.resolve(c.bg)) == fill {
                x += 1;
            }

            out.push_str(&format!(
                "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{}\"/>\n",
                start * CELL_W,
                y * CELL_H,
                (x - start) * CELL_W,
                CELL_H,
                fill.unwrap(),
            ));
        }
    }

    // Box-drawing glyphs as vector strokes from the cell center to its edges, so
    // they meet across cell boundaries (a glyph at a 13px baseline in a 17px row
    // would not). `─`/`│` span the full cell, joining their neighbours.
    for y in 0..rows {
        for x in 0..cols {
            let Some(here) = cell(x, y) else { continue };
            let Some([up, right, down, left]) = box_segments(here.symbol()) else {
                continue;
            };

            let stroke = text_fill(here, &scheme);
            let (cx, cy) = (x * CELL_W + CELL_W / 2, y * CELL_H + CELL_H / 2);
            let (x0, x1, y0, y1) = (x * CELL_W, (x + 1) * CELL_W, y * CELL_H, (y + 1) * CELL_H);

            let mut segment = |ax, ay, bx, by| {
                out.push_str(&format!(
                    "<line x1=\"{ax}\" y1=\"{ay}\" x2=\"{bx}\" y2=\"{by}\" \
stroke=\"{stroke}\" stroke-width=\"1\"/>\n"
                ));
            };

            if up {
                segment(cx, y0, cx, cy);
            }

            if right {
                segment(cx, cy, x1, cy);
            }

            if down {
                segment(cx, cy, cx, y1);
            }

            if left {
                segment(x0, cy, cx, cy);
            }
        }
    }

    let renderable = |c: Option<&ratatui::buffer::Cell>| {
        let symbol = c.map(|c| c.symbol()).unwrap_or(" ");
        symbol != " " && !symbol.is_empty() && box_segments(symbol).is_none()
    };

    for y in 0..rows {
        let mut x = 0;

        while x < cols {
            if !renderable(cell(x, y)) {
                x += 1;
                continue;
            }

            let style = cell(x, y).map(|c| (text_fill(c, &scheme), text_attrs(c)));
            let start = x;
            let mut run = String::new();

            while x < cols {
                let here = cell(x, y);
                let here_style = here.map(|c| (text_fill(c, &scheme), text_attrs(c)));

                if !renderable(here) || here_style != style {
                    break;
                }

                run.push_str(here.map(|c| c.symbol()).unwrap_or(" "));
                x += 1;
            }

            let (fill, attrs) = style.unwrap();

            out.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" fill=\"{fill}\"{attrs} textLength=\"{}\" \
lengthAdjust=\"spacingAndGlyphs\" xml:space=\"preserve\">{}</text>\n",
                start * CELL_W,
                y * CELL_H + BASELINE,
                (x - start) * CELL_W,
                escape(&run),
            ));
        }
    }

    out.push_str("</svg>\n");
    out
}

/// The half-segments `[up, right, down, left]` a light box-drawing glyph draws
/// from the cell center, or `None` if `symbol` isn't one we stroke as vectors.
fn box_segments(symbol: &str) -> Option<[bool; 4]> {
    let segments = match symbol {
        "─" => [false, true, false, true],
        "│" => [true, false, true, false],
        "┌" => [false, true, true, false],
        "┐" => [false, false, true, true],
        "└" => [true, true, false, false],
        "┘" => [true, false, false, true],
        "├" => [true, true, true, false],
        "┤" => [true, false, true, true],
        "┬" => [false, true, true, true],
        "┴" => [true, true, false, true],
        "┼" => [true, true, true, true],
        _ => return None,
    };

    Some(segments)
}

/// The text fill for a cell: its foreground, or the canvas default for
/// `Color::Reset`.
fn text_fill(cell: &ratatui::buffer::Cell, scheme: &Scheme) -> String {
    scheme
        .resolve(cell.fg)
        .unwrap_or_else(|| scheme.fg.to_string())
}

/// SVG attributes for a cell's text modifiers (bold/italic/dim).
fn text_attrs(cell: &ratatui::buffer::Cell) -> String {
    let mut attrs = String::new();

    if cell.modifier.contains(Modifier::BOLD) {
        attrs.push_str(" font-weight=\"bold\"");
    }

    if cell.modifier.contains(Modifier::ITALIC) {
        attrs.push_str(" font-style=\"italic\"");
    }

    if cell.modifier.contains(Modifier::DIM) {
        attrs.push_str(" fill-opacity=\"0.6\"");
    }

    attrs
}

/// Escape the XML metacharacters that can appear in glyph text.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn one_cell(symbol: &str, fg: Color, bg: Color) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        buffer[(0, 0)].set_symbol(symbol).set_fg(fg).set_bg(bg);
        buffer
    }

    #[test]
    fn ansi_foreground_maps_to_scheme_rgb() {
        let svg = buffer_to_svg(&one_cell("+", Color::Green, Color::Reset), ThemeMode::Dark);
        assert!(svg.contains("fill=\"#3fb950\""), "{svg}");
        assert!(svg.contains(">+</text>"), "{svg}");
    }

    #[test]
    fn rgb_background_passes_through_as_a_rect() {
        let svg = buffer_to_svg(
            &one_cell(" ", Color::Reset, Color::Rgb(18, 48, 32)),
            ThemeMode::Dark,
        );
        assert!(svg.contains("<rect x=\"0\" y=\"0\""), "{svg}");
        assert!(svg.contains("fill=\"#123020\""), "{svg}");
    }

    #[test]
    fn reset_background_draws_no_cell_rect() {
        // Only the full-canvas background rect, no per-cell rect.
        let svg = buffer_to_svg(&one_cell("a", Color::Reset, Color::Reset), ThemeMode::Dark);
        assert_eq!(svg.matches("<rect").count(), 1, "{svg}");
    }

    #[test]
    fn box_drawing_is_stroked_not_texted() {
        // `│` becomes a full-height vertical stroke at the cell center, with no
        // text element, so it connects across rows.
        let svg = buffer_to_svg(&one_cell("│", Color::Yellow, Color::Reset), ThemeMode::Dark);
        assert!(svg.contains("<line"), "{svg}");
        assert!(!svg.contains("<text"), "{svg}");
        let cx = CELL_W / 2;
        // Two segments meeting at the center span the full cell height.
        assert!(svg.contains(&format!("x1=\"{cx}\" y1=\"0\"")), "{svg}");
        assert!(svg.contains(&format!("y2=\"{}\"", CELL_H)), "{svg}");
    }

    #[test]
    fn metacharacters_are_escaped() {
        let svg = buffer_to_svg(&one_cell("<", Color::Reset, Color::Reset), ThemeMode::Dark);
        assert!(svg.contains(">&lt;</text>"), "{svg}");
    }

    #[test]
    fn dimensions_follow_the_buffer() {
        let buffer = Buffer::empty(Rect::new(0, 0, 10, 3));
        let svg = buffer_to_svg(&buffer, ThemeMode::Light);
        assert!(svg.contains(&format!("width=\"{}\"", 10 * CELL_W)), "{svg}");
        assert!(svg.contains(&format!("height=\"{}\"", 3 * CELL_H)), "{svg}");
        assert!(svg.contains("fill=\"#ffffff\""), "{svg}");
    }
}
