//! The login QR code, drawn by us rather than by the `qrcode` crate's stock
//! renderer: round dots, rounded finder patterns and the tg-mcp mark in the
//! middle on a white plate. Error correction H keeps the code readable with
//! its centre covered. Also the loading placeholder in the same grid, so the
//! code appears to settle out of it.

use std::fmt::Write as _;

use qrcode::{Color, EcLevel, QrCode};

/// Ink of the dots, finders and mark.
const INK: &str = "#3D2C6E";
/// The dot accent of the mark.
const ACCENT: &str = "#E4B2EA";
/// White margin around the code, in modules (the spec asks for 4; the plate
/// and the page around it give the rest).
const QUIET: usize = 2;
/// A dot's radius, in modules.
const DOT: f64 = 0.42;
/// Side of the loader's grid, in modules.
const LOADER: usize = 25;

/// `data` as a styled SVG QR code; `None` when it doesn't fit a QR code.
pub fn styled_svg(data: &str) -> Option<String> {
    let code = QrCode::with_error_correction_level(data, EcLevel::H).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    let size = width + 2 * QUIET;
    let logo = logo_side(width);

    let mut dots = String::new();
    for y in 0..width {
        for x in 0..width {
            if colors[y * width + x] != Color::Dark
                || in_finder(x, y, width)
                || in_logo(x, y, width, logo)
            {
                continue;
            }
            dot(&mut dots, x + QUIET, y + QUIET);
        }
    }

    let mut svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {size} {size}" shape-rendering="geometricPrecision"><rect width="{size}" height="{size}" rx="2.5" fill="#fff"/><path fill="{INK}" d="{dots}"/>"##
    );
    for (fx, fy) in finders(width) {
        finder(&mut svg, fx + QUIET, fy + QUIET, INK);
    }
    let at = QUIET + (width - logo) / 2;
    let _ = write!(
        svg,
        r##"<svg x="{x}" y="{y}" width="{logo}" height="{logo}" viewBox="-2 -2 30 30"><rect x="-2" y="-2" width="30" height="30" rx="7" fill="#fff"/><rect x="1.5" y="3.5" width="23" height="17" rx="6" fill="none" stroke="{INK}" stroke-width="2.2"/><path d="M7 20.5 6 24.5l5-4" fill="none" stroke="{INK}" stroke-width="2.2" stroke-linejoin="round"/><circle cx="19" cy="7" r="4" fill="{ACCENT}" stroke="#fff" stroke-width="2"/></svg></svg>"##,
        x = at,
        y = at,
    );
    Some(svg)
}

/// The loading placeholder: a QR-looking grid of dots in a fixed pseudo-random
/// pattern, with a diagonal wave of animation delays (styled by the page's
/// `.qr-load` rules), the three finders and the mark's outline in the middle.
pub fn loader_svg() -> String {
    let logo = logo_side(LOADER);
    let mut seed: u64 = 7;
    let mut svg =
        format!(r#"<svg class="qr-load" viewBox="0 0 {LOADER} {LOADER}" aria-hidden="true">"#);
    for y in 0..LOADER {
        for x in 0..LOADER {
            if in_finder(x, y, LOADER) || in_logo(x, y, LOADER, logo) {
                continue;
            }
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345) % 2_147_483_648;
            if (seed >> 16) & 1 == 0 {
                continue;
            }
            let _ = write!(
                svg,
                r#"<circle cx="{cx}.5" cy="{cy}.5" r="{DOT}" style="animation-delay:{delay}ms"/>"#,
                cx = x,
                cy = y,
                delay = (x + y) * 45,
            );
        }
    }
    for (fx, fy) in finders(LOADER) {
        svg.push_str(r#"<g class="fp">"#);
        finder(&mut svg, fx, fy, "currentColor");
        svg.push_str("</g>");
    }
    let at = (LOADER - logo) / 2;
    let _ = write!(
        svg,
        r#"<rect class="fp" x="{at}.5" y="{at}.5" width="{side}" height="{side}" rx="1.6" fill="none" stroke="currentColor" stroke-width=".5"/></svg>"#,
        side = logo - 1,
    );
    svg
}

/// Side of the cleared square in the middle: about a fifth of the code, odd so
/// it centres on a module, never under 7.
fn logo_side(width: usize) -> usize {
    ((width as f64 * 0.22).round() as usize | 1).max(7)
}

fn finders(width: usize) -> [(usize, usize); 3] {
    [(0, 0), (width - 7, 0), (0, width - 7)]
}

fn in_finder(x: usize, y: usize, width: usize) -> bool {
    finders(width)
        .iter()
        .any(|&(fx, fy)| x >= fx && x < fx + 7 && y >= fy && y < fy + 7)
}

fn in_logo(x: usize, y: usize, width: usize, logo: usize) -> bool {
    let from = (width - logo) / 2;
    (from..from + logo).contains(&x) && (from..from + logo).contains(&y)
}

/// One round dot centred on the module at (`x`, `y`), as path commands.
fn dot(out: &mut String, x: usize, y: usize) {
    let _ = write!(
        out,
        "M{x}.5 {y}.5m-{DOT} 0a{DOT} {DOT} 0 1 0 {d} 0a{DOT} {DOT} 0 1 0-{d} 0",
        d = 2.0 * DOT,
    );
}

/// A finder pattern with its top-left module at (`x`, `y`): a rounded ring and
/// a rounded 3×3 centre.
fn finder(out: &mut String, x: usize, y: usize, ink: &str) {
    let _ = write!(
        out,
        r#"<rect x="{x}.5" y="{y}.5" width="6" height="6" rx="1.6" fill="none" stroke="{ink}" stroke-width="1"/><rect x="{cx}" y="{cy}" width="3" height="3" rx=".9" fill="{ink}"/>"#,
        cx = x + 2,
        cy = y + 2,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A login URL as long as Telegram's: a 32-byte token in base64url.
    const LOGIN: &str = "tg://login?token=AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA";

    fn dots(svg: &str) -> usize {
        svg.matches("a0.42").count() / 2
    }

    #[test]
    fn every_dark_module_outside_the_finders_and_the_mark_is_a_dot() {
        let code = QrCode::with_error_correction_level(LOGIN, EcLevel::H).unwrap();
        let width = code.width();
        let logo = logo_side(width);
        let colors = code.to_colors();
        let expected = (0..width * width)
            .filter(|i| {
                let (x, y) = (i % width, i / width);
                colors[*i] == Color::Dark && !in_finder(x, y, width) && !in_logo(x, y, width, logo)
            })
            .count();

        let svg = styled_svg(LOGIN).unwrap();
        assert_eq!(dots(&svg), expected);
        assert_eq!(svg.matches(r#"width="6" height="6" rx="1.6""#).count(), 3);
        assert!(svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(svg.ends_with("</svg></svg>"));
    }

    #[test]
    fn the_mark_covers_little_enough_for_error_correction_h() {
        let code = QrCode::with_error_correction_level(LOGIN, EcLevel::H).unwrap();
        let width = code.width();
        let logo = logo_side(width);
        assert!(logo % 2 == 1 && logo >= 7, "{logo}");
        // H restores 30% of the codewords; the mark hides well under a third of that.
        assert!(
            (logo * logo) as f64 / (width * width) as f64 <= 0.08,
            "{logo} of {width}"
        );
        assert_eq!(code.error_correction_level(), EcLevel::H);
    }

    #[test]
    fn data_too_long_for_a_qr_code_is_none() {
        assert!(styled_svg(&"x".repeat(4000)).is_none());
    }

    #[test]
    fn the_loader_is_the_same_grid_with_a_diagonal_wave() {
        let svg = loader_svg();
        assert!(svg.starts_with(r#"<svg class="qr-load""#));
        assert!(svg.contains(r#"aria-hidden="true""#));
        let circles = svg.matches("<circle ").count();
        assert!((200..400).contains(&circles), "{circles}");
        // Each dot's delay grows with its distance from the top-left corner.
        for circle in svg.split("<circle ").skip(1) {
            let number = |key: &str| -> usize {
                let rest = &circle[circle.find(key).expect(key) + key.len()..];
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().unwrap()
            };
            assert_eq!(
                number("animation-delay:"),
                (number("cx=\"") + number("cy=\"")) * 45,
                "{circle}"
            );
        }
        assert_eq!(svg.matches(r#"<g class="fp">"#).count(), 3);
        // Fixed pattern: the same every time, so the page doesn't flicker.
        assert_eq!(svg, loader_svg());
    }
}
