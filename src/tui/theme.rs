//! Light/dark theming (PRD §11.1).
//!
//! Detection order: explicit choice → OSC 11 background query → `COLORFGBG`
//! hint → dark fallback. The OSC 11 query asks the terminal for its background
//! color and classifies it by luminance, so a light terminal is honored even
//! when `COLORFGBG` is unset. Every step is tolerant: a failed query degrades to
//! the next source.
//!
//! The OSC 11 query is paired with a Device Attributes request (DA1, `ESC [ c`)
//! as a sync barrier. Terminals answer DA1 in order and near-universally, so the
//! arrival of the DA1 reply proves the OSC 11 reply (if any) has already been
//! delivered — making detection independent of a guessed timeout. Reading
//! through the DA1 reply also consumes it so it never leaks into the key loop.
//!
//! Colors are ANSI-first so they track the user's terminal theme: foregrounds
//! use the 16 named ANSI colors and the default fg/bg inherit the terminal
//! (`Reset`). Only the diff and highlight *backgrounds* keep a faint per-mode
//! tint, since no ANSI slot provides a subtle background.

use std::io::{IsTerminal, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use ratatui::style::Color;

/// Whether to render for a light or dark terminal background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Light,
    Dark,
}

impl ThemeMode {
    /// Resolve the effective mode from an optional explicit choice, then the
    /// terminal's reported background, then `COLORFGBG`, then dark. Must be
    /// called with the terminal in raw mode so the OSC 11 and DA1 replies can be
    /// read.
    pub fn resolve(explicit: Option<ThemeMode>) -> ThemeMode {
        explicit
            .or_else(query_terminal_background)
            .or_else(Self::from_colorfgbg)
            .unwrap_or(ThemeMode::Dark)
    }

    /// Interpret `COLORFGBG` (e.g. `"15;0"`): a low background index is dark.
    fn from_colorfgbg() -> Option<ThemeMode> {
        let value = std::env::var("COLORFGBG").ok()?;
        let background: u8 = value.rsplit(';').next()?.trim().parse().ok()?;

        match background {
            0..=6 | 8 => Some(ThemeMode::Dark),
            _ => Some(ThemeMode::Light),
        }
    }
}

/// Classify an sRGB background color by perceived luminance.
fn mode_from_rgb(r: u8, g: u8, b: u8) -> ThemeMode {
    let luma = 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);

    if luma < 128.0 {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    }
}

/// Ask the terminal for its background color via OSC 11 and classify it.
/// Returns `None` when not attached to a terminal or the query fails/times out.
fn query_terminal_background() -> Option<ThemeMode> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return None;
    }

    let reply = osc11_reply()?;
    let (r, g, b) = parse_osc11(&reply)?;
    Some(mode_from_rgb(r, g, b))
}

/// Query the terminal background (OSC 11) and read until the DA1 reply that
/// follows it, which bounds the read without relying on a guessed timeout. The
/// outer deadline only guards terminals that answer neither request.
fn osc11_reply() -> Option<String> {
    let mut out = std::io::stdout();
    out.write_all(b"\x1b]11;?\x07\x1b[c").ok()?;
    out.flush().ok()?;

    // Read straight from the fd: a buffered reader (e.g. `stdin().lock()`) would
    // slurp the whole reply into userspace on the first read, leaving `poll`
    // below to see an empty kernel buffer and block until the deadline.
    let fd = std::io::stdin().as_raw_fd();

    let mut buf = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(500);

    while Instant::now() < deadline && buf.len() < 256 {
        let remaining = deadline.saturating_duration_since(Instant::now());

        if !fd_readable(fd, remaining) {
            break;
        }

        match read_byte(fd) {
            Some(byte) => buf.push(byte),
            None => break,
        }

        // The DA1 reply is the sync barrier: once it arrives, any OSC 11 reply
        // has already been read, so we can stop without waiting out the deadline.
        if ends_with_da1(&buf) {
            break;
        }
    }

    (!buf.is_empty()).then(|| String::from_utf8_lossy(&buf).into_owned())
}

/// Read a single byte directly from `fd`, bypassing buffering. `None` on EOF or
/// error.
fn read_byte(fd: RawFd) -> Option<u8> {
    let mut byte = 0u8;

    // SAFETY: `byte` is a valid, writable single-byte buffer for the call.
    let read = unsafe { libc::read(fd, std::ptr::from_mut(&mut byte).cast(), 1) };

    (read == 1).then_some(byte)
}

/// Whether `buf` ends with a complete DA1 reply (`ESC [ … c`, parameters limited
/// to digits, `;`, and `?`).
fn ends_with_da1(buf: &[u8]) -> bool {
    if buf.last() != Some(&b'c') {
        return false;
    }

    match buf.windows(2).rposition(|pair| pair == b"\x1b[") {
        Some(start) => buf[start + 2..buf.len() - 1]
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b'?')),
        None => false,
    }
}

/// True when `fd` has data ready within `timeout`.
fn fd_readable(fd: RawFd, timeout: Duration) -> bool {
    let mut poll_fd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;

    // SAFETY: `poll_fd` is a single valid `pollfd` living for the call's duration.
    let ready = unsafe { libc::poll(&mut poll_fd, 1, millis) };
    ready > 0 && (poll_fd.revents & libc::POLLIN) != 0
}

/// Parse `rgb:rrrr/gggg/bbbb` out of an OSC 11 reply into an sRGB triple.
fn parse_osc11(reply: &str) -> Option<(u8, u8, u8)> {
    let rest = reply.split("rgb:").nth(1)?;
    let mut parts = rest.split('/');

    let r = parse_hex_component(parts.next()?)?;
    let g = parse_hex_component(parts.next()?)?;
    let b = parse_hex_component(parts.next()?)?;

    Some((r, g, b))
}

/// Parse one hex color component (2- or 4-digit) and scale it to 8 bits.
fn parse_hex_component(value: &str) -> Option<u8> {
    let hex: String = value.chars().take_while(char::is_ascii_hexdigit).collect();

    if hex.is_empty() {
        return None;
    }

    let parsed = u32::from_str_radix(&hex, 16).ok()?;
    let max = (1u32 << (4 * hex.len() as u32)) - 1;
    Some((parsed * 255 / max) as u8)
}

/// Colors for diff semantics and UI chrome. Foreground accents are ANSI named
/// colors (theme-tracking); only the backgrounds carry a faint per-mode tint.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Background for added lines.
    pub add_bg: Color,
    /// Background for removed lines.
    pub remove_bg: Color,
    /// Background for the visual selection.
    pub selection_bg: Color,
    /// Background highlight for the focused row.
    pub cursor_bg: Color,
    /// Tint for a source line that carries an annotation.
    pub annotated_line_bg: Color,
    /// Background for an inline annotation block (distinct from add/remove).
    pub annotation_bg: Color,
    /// Foreground for line-number gutters and secondary text.
    pub gutter_fg: Color,
    /// Default foreground (inherits the terminal).
    pub default_fg: Color,
    /// Foreground for an added line's `+` sign.
    pub sign_add: Color,
    /// Foreground for a removed line's `-` sign.
    pub sign_remove: Color,
    /// Foreground for hunk headers.
    pub hunk_fg: Color,
    /// Foreground for the keys in the help bar.
    pub help_key: Color,
    /// Foreground for an open-annotation marker.
    pub marker_open: Color,
    /// Foreground for a resolved-annotation marker.
    pub marker_resolved: Color,
    /// Foreground for an orphaned/attention marker.
    pub marker_attention: Color,
}

impl Palette {
    /// The palette for `mode`.
    pub fn for_mode(mode: ThemeMode) -> Self {
        let backgrounds = match mode {
            ThemeMode::Dark => Backgrounds {
                add_bg: Color::Rgb(18, 48, 32),
                remove_bg: Color::Rgb(58, 24, 28),
                selection_bg: Color::Rgb(46, 52, 64),
                cursor_bg: Color::Rgb(38, 42, 52),
                annotated_line_bg: Color::Rgb(40, 38, 22),
                annotation_bg: Color::Rgb(54, 50, 28),
            },
            ThemeMode::Light => Backgrounds {
                add_bg: Color::Rgb(214, 245, 222),
                remove_bg: Color::Rgb(250, 220, 222),
                selection_bg: Color::Rgb(216, 224, 240),
                cursor_bg: Color::Rgb(226, 232, 242),
                annotated_line_bg: Color::Rgb(250, 246, 214),
                annotation_bg: Color::Rgb(246, 238, 198),
            },
        };

        Palette {
            add_bg: backgrounds.add_bg,
            remove_bg: backgrounds.remove_bg,
            selection_bg: backgrounds.selection_bg,
            cursor_bg: backgrounds.cursor_bg,
            annotated_line_bg: backgrounds.annotated_line_bg,
            annotation_bg: backgrounds.annotation_bg,
            gutter_fg: Color::DarkGray,
            default_fg: Color::Reset,
            sign_add: Color::Green,
            sign_remove: Color::Red,
            hunk_fg: Color::Cyan,
            help_key: Color::Magenta,
            marker_open: Color::Yellow,
            marker_resolved: Color::Green,
            marker_attention: Color::Red,
        }
    }
}

/// The per-mode background tints, kept together so [`Palette::for_mode`] reads
/// as "these backgrounds, those ANSI foregrounds".
struct Backgrounds {
    add_bg: Color,
    remove_bg: Color,
    selection_bg: Color,
    cursor_bg: Color,
    annotated_line_bg: Color,
    annotation_bg: Color,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_choice_wins() {
        assert_eq!(ThemeMode::resolve(Some(ThemeMode::Light)), ThemeMode::Light);
        assert_eq!(ThemeMode::resolve(Some(ThemeMode::Dark)), ThemeMode::Dark);
    }

    #[test]
    fn colorfgbg_dark_background_is_dark() {
        std::env::set_var("COLORFGBG", "15;0");
        assert_eq!(ThemeMode::from_colorfgbg(), Some(ThemeMode::Dark));
        std::env::set_var("COLORFGBG", "0;15");
        assert_eq!(ThemeMode::from_colorfgbg(), Some(ThemeMode::Light));
        std::env::remove_var("COLORFGBG");
    }

    #[test]
    fn osc11_reply_classifies_by_luminance() {
        // A near-black background is dark; a near-white one is light.
        assert_eq!(
            parse_osc11("\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\").map(|(r, g, b)| mode_from_rgb(r, g, b)),
            Some(ThemeMode::Dark)
        );
        assert_eq!(
            parse_osc11("\x1b]11;rgb:ffff/ffff/ffff\x07").map(|(r, g, b)| mode_from_rgb(r, g, b)),
            Some(ThemeMode::Light)
        );
    }

    #[test]
    fn parses_two_digit_components() {
        assert_eq!(parse_osc11("rgb:00/80/ff"), Some((0, 128, 255)));
    }

    #[test]
    fn parses_osc11_preceding_the_da1_barrier() {
        // The read keeps the whole stream through DA1; the color is still found.
        let reply = "\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?6c";
        assert_eq!(
            parse_osc11(reply).map(|(r, g, b)| mode_from_rgb(r, g, b)),
            Some(ThemeMode::Light)
        );
    }

    #[test]
    fn da1_barrier_recognizes_only_complete_replies() {
        assert!(ends_with_da1(b"\x1b[?6c"));
        assert!(ends_with_da1(b"\x1b]11;rgb:cccc/cccc/cccc\x1b\\\x1b[?1;2c"));
        // A partial color reply ending in a hex `c` is not the barrier.
        assert!(!ends_with_da1(b"\x1b]11;rgb:cccc/cccc/cccc"));
        assert!(!ends_with_da1(b"\x1b[?6"));
    }
}
