// SPDX-License-Identifier: AGPL-3.0-only
//! ANSI text to styled spans with `vte`. Output carries text and style only (PLAN §5.4).

/// A terminal color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// One of the 16 standard colors (0–7 normal, 8–15 bright).
    Ansi(u8),
    /// A 256-color palette entry (16–255).
    Indexed(u8),
    /// A 24-bit color.
    Rgb(u8, u8, u8),
}

/// Text style carried by a span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    /// Foreground color.
    pub fg: Option<Color>,
    /// Background color.
    pub bg: Option<Color>,
    /// Bold weight.
    pub bold: bool,
    /// Italic.
    pub italic: bool,
    /// Underline.
    pub underline: bool,
}

/// A run of text with one style. Never contains control characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// The text, to be rendered as a text node.
    pub text: String,
    /// Its style.
    pub style: Style,
}

/// Parses `input` into spans.
#[must_use]
pub fn parse(input: &str) -> Vec<Span> {
    let mut sink = Sink::default();
    let mut parser = vte::Parser::<0>::new_with_size();
    parser.advance(&mut sink, input.as_bytes());
    sink.finish()
}

/// Collects printable text and SGR state; every other sequence is ignored by `vte`'s defaults.
#[derive(Default)]
struct Sink {
    spans: Vec<Span>,
    text: String,
    style: Style,
}

impl Sink {
    fn flush(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        match self.spans.last_mut() {
            Some(last) if last.style == self.style => last.text.push_str(&text),
            _ => self.spans.push(Span {
                text,
                style: self.style,
            }),
        }
    }

    fn finish(mut self) -> Vec<Span> {
        self.flush();
        self.spans
    }

    fn sgr(&mut self, params: &vte::Params) {
        self.flush();
        let groups: Vec<&[u16]> = params.iter().collect();
        if groups.is_empty() {
            self.style = Style::default();
        }
        let mut i = 0;
        while let Some(group) = groups.get(i).copied() {
            i += 1;
            let code = group.first().copied().unwrap_or(0);
            match code {
                0 => self.style = Style::default(),
                1 => self.style.bold = true,
                3 => self.style.italic = true,
                4 => self.style.underline = true,
                22 => self.style.bold = false,
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                30..=37 => self.style.fg = Some(Color::Ansi(narrow(code - 30))),
                39 => self.style.fg = None,
                40..=47 => self.style.bg = Some(Color::Ansi(narrow(code - 40))),
                49 => self.style.bg = None,
                90..=97 => self.style.fg = Some(Color::Ansi(narrow(code - 90 + 8))),
                100..=107 => self.style.bg = Some(Color::Ansi(narrow(code - 100 + 8))),
                38 | 48 => {
                    let (color, used) = extended(group, groups.get(i..).unwrap_or_default());
                    i += used;
                    if let Some(color) = color {
                        if code == 38 {
                            self.style.fg = Some(color);
                        } else {
                            self.style.bg = Some(color);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn narrow(v: u16) -> u8 {
    u8::try_from(v).unwrap_or(u8::MAX)
}

fn palette(n: u16) -> Option<Color> {
    let n = u8::try_from(n).ok()?;
    Some(if n < 16 {
        Color::Ansi(n)
    } else {
        Color::Indexed(n)
    })
}

/// Decodes `38`/`48` extended colors. Returns the color and how many following groups
/// (semicolon form) were consumed.
fn extended(group: &[u16], rest: &[&[u16]]) -> (Option<Color>, usize) {
    let channel = |v: u16| u8::try_from(v).ok();
    if group.len() > 1 {
        // Colon form: `38:5:n` or `38:2:[colorspace:]r:g:b`.
        let color = match group.get(1) {
            Some(5) => group.get(2).and_then(|n| palette(*n)),
            Some(2) if group.len() >= 5 => {
                let n = group.len();
                match (group.get(n - 3), group.get(n - 2), group.get(n - 1)) {
                    (Some(r), Some(g), Some(b)) => channel(*r)
                        .zip(channel(*g))
                        .zip(channel(*b))
                        .map(|((r, g), b)| Color::Rgb(r, g, b)),
                    _ => None,
                }
            }
            _ => None,
        };
        return (color, 0);
    }
    let first = |i: usize| rest.get(i).and_then(|g| g.first()).copied();
    match first(0) {
        Some(5) => (first(1).and_then(palette), 2.min(rest.len())),
        Some(2) => {
            let color = match (first(1), first(2), first(3)) {
                (Some(r), Some(g), Some(b)) => channel(r)
                    .zip(channel(g))
                    .zip(channel(b))
                    .map(|((r, g), b)| Color::Rgb(r, g, b)),
                _ => None,
            };
            (color, 4.min(rest.len()))
        }
        _ => (None, 0),
    }
}

impl vte::Perform for Sink {
    fn print(&mut self, c: char) {
        if !c.is_control() {
            self.text.push(c);
        }
    }

    fn execute(&mut self, byte: u8) {
        if byte == b'\t' {
            self.text.push('\t');
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if action == 'm' && !ignore && intermediates.is_empty() {
            self.sgr(params);
        }
    }
}

/// CSS color for `color`: a custom property for the 16 standard colors (so themes can set them),
/// a hex value otherwise. Applied through the CSSOM, never an inline `style` attribute (CSP).
#[must_use]
pub fn css_color(color: Color) -> String {
    match color {
        Color::Ansi(n) => format!("var(--ansi-{n})"),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(n) => {
            let (r, g, b) = if n >= 232 {
                let v = 8 + 10 * (n - 232);
                (v, v, v)
            } else {
                const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
                let i = usize::from(n.saturating_sub(16));
                let at = |k: usize| LEVELS.get(k).copied().unwrap_or(0);
                (at(i / 36), at(i / 6 % 6), at(i % 6))
            };
            format!("#{r:02x}{g:02x}{b:02x}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str, style: Style) -> Span {
        Span {
            text: text.to_owned(),
            style,
        }
    }

    fn plain(text: &str) -> Span {
        span(text, Style::default())
    }

    #[test]
    fn css_colors_for_each_kind() {
        assert_eq!(css_color(Color::Ansi(9)), "var(--ansi-9)");
        assert_eq!(css_color(Color::Rgb(1, 171, 255)), "#01abff");
        // xterm cube: 208 = 16 + 36*5 + 6*2 + 0 -> (255, 135, 0); 232 -> gray 8; 255 -> gray 238.
        assert_eq!(css_color(Color::Indexed(208)), "#ff8700");
        assert_eq!(css_color(Color::Indexed(232)), "#080808");
        assert_eq!(css_color(Color::Indexed(255)), "#eeeeee");
        assert_eq!(css_color(Color::Indexed(16)), "#000000");
        assert_eq!(css_color(Color::Indexed(231)), "#ffffff");
    }

    #[test]
    fn plain_text_is_one_unstyled_span() {
        assert_eq!(parse("hello"), vec![plain("hello")]);
    }

    #[test]
    fn sixteen_colors_foreground_background_and_bright() {
        let got = parse("\x1b[31mred\x1b[0m \x1b[42mgreen-bg\x1b[0m\x1b[93mbright\x1b[m");
        let fg = |c| Style {
            fg: Some(c),
            ..Style::default()
        };
        assert_eq!(got[0], span("red", fg(Color::Ansi(1))));
        assert_eq!(got[1], plain(" "));
        assert_eq!(
            got[2],
            span(
                "green-bg",
                Style {
                    bg: Some(Color::Ansi(2)),
                    ..Style::default()
                }
            )
        );
        assert_eq!(got[3], span("bright", fg(Color::Ansi(11))));
    }

    #[test]
    fn palette_256_and_truecolor_in_semicolon_and_colon_forms() {
        let got = parse("\x1b[38;5;208ma\x1b[48;5;3mb\x1b[38;2;1;2;3mc\x1b[38:2::9:8:7md");
        assert_eq!(got[0].style.fg, Some(Color::Indexed(208)));
        assert_eq!(got[1].style.bg, Some(Color::Ansi(3)));
        assert_eq!(got[1].style.fg, Some(Color::Indexed(208)));
        assert_eq!(got[2].style.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(got[3].style.fg, Some(Color::Rgb(9, 8, 7)));
    }

    #[test]
    fn bold_italic_underline_and_their_resets() {
        let got = parse("\x1b[1;3;4mx\x1b[22my\x1b[23mz\x1b[24mw");
        let all = Style {
            bold: true,
            italic: true,
            underline: true,
            ..Style::default()
        };
        assert_eq!(got[0], span("x", all));
        assert!(!got[1].style.bold && got[1].style.italic);
        assert!(!got[2].style.italic && got[2].style.underline);
        assert_eq!(got[3], plain("w"));
    }

    #[test]
    fn reset_clears_everything_and_default_color_codes_clear_one() {
        let got = parse("\x1b[1;31ma\x1b[39mb\x1b[0mc");
        assert_eq!(got[1].style.fg, None);
        assert!(got[1].style.bold);
        assert_eq!(got[2], plain("c"));
    }

    #[test]
    fn cursor_erase_osc_and_private_sequences_are_stripped() {
        let got = parse("a\x1b[2K\x1b[1A\x1b[10;5Hb\x1b]0;title\x07c\x1b[?25ld\x1b[?1mx");
        assert_eq!(got, vec![plain("abcdx")]);
    }

    #[test]
    fn control_characters_other_than_tab_are_dropped() {
        assert_eq!(parse("a\r\x08b\x07\tc\x00"), vec![plain("ab\tc")]);
    }

    #[test]
    fn malformed_and_partial_sequences_never_leak_escape_bytes() {
        for input in [
            "\x1b",
            "\x1b[",
            "\x1b[31",
            "\x1b[38;2;1m",
            "\x1b[999999999999999m",
            "\x1b[38;5mx",
            "\x1b[;;;;m y",
            "\x1b]unterminated osc",
            "\x1bP1;2|data",
            "a\x1b[\x1bb",
            "\u{9b}31mx",
        ] {
            for s in parse(input) {
                assert!(
                    !s.text.chars().any(|c| c.is_control() && c != '\t'),
                    "{input:?} leaked {:?}",
                    s.text
                );
            }
        }
        assert_eq!(parse("\x1b[31"), Vec::<Span>::new());
        assert_eq!(
            parse("a\x1b[\x1bb"),
            vec![plain("a")],
            "ESC restarts; ESC b is a complete escape"
        );
    }

    #[test]
    fn arbitrary_bytes_do_not_panic() {
        let mut x: u32 = 12345;
        let mut s = String::new();
        for _ in 0..20_000 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let pick = (x >> 16) % 8;
            s.push(match pick {
                0 => '\x1b',
                1 => '[',
                2 => ';',
                3 => 'm',
                4 => char::from(b'0' + u8::try_from((x >> 8) % 10).unwrap()),
                5 => ':',
                6 => '\u{9b}',
                _ => 'q',
            });
        }
        let _ = parse(&s);
    }

    fn fixture(name: &str) -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/maestro/2.12.1/"
        );
        String::from_utf8_lossy(&std::fs::read(format!("{path}{name}/stdout.bin")).unwrap())
            .into_owned()
    }

    #[test]
    fn real_maestro_tty_output_has_no_escape_bytes_and_keeps_its_text() {
        for name in ["tty-ansi", "verbose"] {
            let raw = fixture(name);
            assert!(raw.contains('\x1b'), "{name} should contain ANSI");
            let spans = parse(&raw);
            let text: String = spans.iter().map(|s| s.text.as_str()).collect();
            assert!(!text.contains('\x1b'), "{name}");
            assert!(text.contains("Waiting for flows to complete") || name == "verbose");
        }
        let spans = parse(&fixture("tty-ansi"));
        assert!(
            spans.iter().any(|s| s.style.fg == Some(Color::Ansi(6))),
            "ESC[36m is cyan"
        );
    }
}
