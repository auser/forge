//! `NO_COLOR`/dumb-terminal detection and the five ANSI escapes the
//! interactive chat is allowed to use (design §2.4, §12.2).
//!
//! Decided once, at startup, from the environment plus `--no-color`; never
//! re-checked mid-session. Colour is applied only by the writer — every
//! [`forge_chat::render`] test asserts on `Line::text` alone, so a palette
//! change here cannot break one.
//!
//! No styling crate: the design deliberately caps this at five hand-written
//! escapes (`\x1b[2m`, `\x1b[31m`, `\x1b[32m`, `\x1b[33m`, `\x1b[36m`) plus
//! `\x1b[0m` to reset, for six non-`Plain` [`Style`] variants — so
//! [`Style::Notice`] (a background job's out-of-band news) shares the dim
//! escape with [`Style::Meta`] rather than a design that would need a
//! sixth colour: both are secondary information, never the thing the user
//! asked for.

use std::io::IsTerminal;

use forge_chat::Style;

const RESET: &str = "\x1b[0m";

/// Whether transcript lines get ANSI colour, and how to apply it.
///
/// A plain `bool` would work just as well today, but this is a `struct` (not
/// a type alias) so that a future terminal-capability nuance (256-colour vs
/// 16, say) has somewhere to live without changing every call site.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Palette {
    enabled: bool,
}

impl Palette {
    /// Colour is on iff stdout is a TTY, `NO_COLOR` is unset, `--no-color`
    /// was not passed, and `TERM` is set and not `dumb` (design §12.2).
    /// `TERM` unset counts the same as `TERM=dumb`: with no terminfo entry
    /// to trust, assuming colour support is exactly the guess `NO_COLOR`
    /// exists to avoid.
    pub fn detect(no_color_flag: bool) -> Self {
        let is_tty = std::io::stdout().is_terminal();
        let no_color_env = std::env::var_os("NO_COLOR").is_some();
        let term_is_usable = std::env::var("TERM").is_ok_and(|term| term != "dumb");
        Self {
            enabled: is_tty && !no_color_env && !no_color_flag && term_is_usable,
        }
    }

    /// Force colour on, bypassing environment detection entirely. Behind
    /// `#[cfg(test)]` so nothing outside this module's tests can reach for
    /// it as a shortcut around real detection.
    #[cfg(test)]
    pub fn forced_on_for_tests() -> Self {
        Self { enabled: true }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The `ColorMode` to hand to rustyline, so the terminal implementation
    /// and this palette can never disagree about whether colour is on
    /// (design §12.2).
    pub fn color_mode(&self) -> rustyline::ColorMode {
        if self.enabled {
            rustyline::ColorMode::Enabled
        } else {
            rustyline::ColorMode::Disabled
        }
    }

    /// Wrap `text` in `style`'s escape, or return it unchanged when colour
    /// is off or `style` is [`Style::Plain`] — the one class that is never
    /// decorated because it is the answer the user asked for, not
    /// commentary about it.
    pub fn paint(&self, style: Style, text: &str) -> String {
        match ansi(style) {
            Some(escape) if self.enabled => format!("{escape}{text}{RESET}"),
            _ => text.to_string(),
        }
    }
}

/// The design's five escapes (§2.4), one per non-`Plain` style; `Notice`
/// doubles up on `Meta`'s rather than adding a sixth.
fn ansi(style: Style) -> Option<&'static str> {
    match style {
        Style::Plain => None,
        Style::Meta | Style::Notice => Some("\x1b[2m"),
        Style::Tool => Some("\x1b[36m"),
        Style::Ok => Some("\x1b[32m"),
        Style::Warn => Some("\x1b[33m"),
        Style::Bad => Some("\x1b[31m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_chat::Style;
    use serial_test::serial;

    fn with_env(vars: &[(&str, Option<&str>)], f: impl FnOnce()) {
        // edition 2024: env mutation is unsafe, and #[serial] keeps it sane.
        let saved: Vec<_> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var(k).ok()))
            .collect();
        for (key, value) in vars {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
        f();
        for (key, value) in saved {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }

    #[test]
    #[serial]
    fn no_color_disables_every_style() {
        with_env(
            &[("NO_COLOR", Some("1")), ("TERM", Some("xterm-256color"))],
            || {
                let p = Palette::detect(false);
                assert!(!p.enabled());
                assert_eq!(p.paint(Style::Bad, "  ! error: boom"), "  ! error: boom");
            },
        );
    }

    #[test]
    #[serial]
    fn a_dumb_terminal_disables_every_style() {
        with_env(&[("NO_COLOR", None), ("TERM", Some("dumb"))], || {
            assert!(!Palette::detect(false).enabled());
        });
    }

    #[test]
    #[serial]
    fn the_flag_alone_disables_every_style() {
        with_env(
            &[("NO_COLOR", None), ("TERM", Some("xterm-256color"))],
            || {
                assert!(!Palette::detect(true).enabled());
            },
        );
    }

    #[test]
    fn an_enabled_palette_wraps_and_always_resets() {
        let p = Palette::forced_on_for_tests();
        let painted = p.paint(Style::Bad, "  ! error: boom");
        assert!(painted.starts_with("\x1b["), "{painted:?}");
        assert!(painted.ends_with("\x1b[0m"), "{painted:?}");
        assert!(painted.contains("  ! error: boom"));
        // Plain text is never decorated, so an answer is never coloured.
        assert_eq!(p.paint(Style::Plain, "the answer"), "the answer");
    }
}
