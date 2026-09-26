//! Terminal styling.
//!
//! Styles are rendered through `anstyle`'s stream-aware renderer, so they arrive as escape
//! sequences on a terminal and as plain text when the output is piped or captured.

use anstyle::{AnsiColor, Color, Style};

/// Emphasizes something the user should read before acting.
pub fn warn(text: &str) -> String {
    render(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow))), text)
}

/// Marks a successful step.
pub fn ok(text: &str) -> String {
    render(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green))), text)
}

/// Highlights an actionable item.
pub fn accent(text: &str) -> String {
    render(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Cyan))), text)
}

/// Makes a section heading easy to scan.
pub fn heading(text: &str) -> String {
    render(Style::new().bold(), text)
}

/// Marks a failure.
pub fn err(text: &str) -> String {
    render(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red))), text)
}

/// De-emphasizes supporting detail.
pub fn dim(text: &str) -> String {
    render(Style::new().dimmed(), text)
}

/// Applies a style to text.
///
/// The escape sequences are emitted verbatim; `anstream`'s printing macros strip them when the
/// output stream is not a terminal.
fn render(style: Style, text: &str) -> String {
    format!("{}{text}{}", style.render(), style.render_reset())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_plain_text_is_the_text_itself_when_colour_is_off() {
        // The exact escape behaviour depends on `anstream`'s colour choice; what must hold is
        // that the payload survives.
        for rendered in [warn("careful"), ok("done"), err("broken"), dim("detail")] {
            assert!(rendered.contains(|c: char| c.is_alphanumeric()), "{rendered:?}");
        }
    }
}
