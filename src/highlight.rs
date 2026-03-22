use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{self, FontStyle, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

use crate::theme::Theme;

/// Lazy-initialized syntax highlighting state.
pub struct Highlighter {
    ss: SyntaxSet,
    theme: highlighting::Theme,
}

impl Highlighter {
    pub fn new() -> Self {
        let ss = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        // base16-eighties.dark is the closest built-in to kanagawa
        let theme = ts.themes["base16-eighties.dark"].clone();
        Self { ss, theme }
    }

    /// Highlight a code block. Returns styled Lines.
    /// `lang` is the fenced code block language tag (e.g. "rust", "python").
    /// Falls back to plain styling if language is unknown.
    pub fn highlight_block(&self, code: &str, lang: Option<&str>) -> Vec<Line<'static>> {
        let syntax = lang
            .and_then(|l| self.ss.find_syntax_by_token(l))
            .unwrap_or_else(|| self.ss.find_syntax_plain_text());

        let mut h = HighlightLines::new(syntax, &self.theme);
        let mut lines = Vec::new();
        let source_lines: Vec<&str> = LinesWithEndings::from(code).collect();
        let total = source_lines.len();
        let gutter_width = total.to_string().len();

        for (i, line) in source_lines.into_iter().enumerate() {
            let line_num = format!("{:>width$} ", i + 1, width = gutter_width);
            let mut all_spans = vec![
                Span::styled(" ", Theme::md_code_block()),
                Span::styled(line_num, Theme::md_code_line_nr()),
            ];

            match h.highlight_line(line, &self.ss) {
                Ok(ranges) => {
                    let spans: Vec<Span<'static>> = ranges
                        .into_iter()
                        .map(|(style, text)| {
                            Span::styled(text.to_string(), to_ratatui_style(style))
                        })
                        .collect();
                    all_spans.extend(spans);
                }
                Err(_) => {
                    let text = line.trim_end_matches('\n');
                    all_spans.push(Span::styled(text.to_string(), Theme::md_code_block()));
                }
            }

            lines.push(Line::from(all_spans));
        }

        lines
    }
}

/// Convert a syntect style to a ratatui Style, using the code block bg.
fn to_ratatui_style(style: highlighting::Style) -> Style {
    let fg = syntect_to_color(style.foreground);
    let mut s = Style::default().fg(fg).bg(Theme::bg_card());
    if style.font_style.contains(FontStyle::BOLD) {
        s = s.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    s
}

fn syntect_to_color(c: highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}
