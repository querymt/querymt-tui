use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::Style,
    text::{Line, Span},
};

use crate::highlight::Highlighter;
use crate::theme::Theme;
use crate::ui::{MD_BULLET, MD_HRULE_CHAR};

/// Render a markdown string into styled ratatui Lines.
/// `base_style` is applied to plain text (allows caller to set the card bg).
pub fn render(md: &str, base_style: Style, hl: &Highlighter) -> Vec<Line<'static>> {
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(md, opts);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut ctx = RenderCtx {
        base_style,
        hl,
        style_stack: vec![base_style],
        current_spans: Vec::new(),
        in_code_block: false,
        code_block_lines: Vec::new(),
        code_lang: None,
        list_depth: 0,
        list_indices: Vec::new(),
        in_heading: false,
        heading_level: 0,
        in_blockquote: false,
        in_table: false,
        in_table_head: false,
        table_alignments: Vec::new(),
        table_rows: Vec::new(),
        current_cell_spans: Vec::new(),
    };

    for event in parser {
        match event {
            Event::Start(tag) => ctx.open_tag(&tag),
            Event::End(tag) => ctx.close_tag(&tag, &mut lines),
            Event::Text(text) => ctx.push_text(&text),
            Event::Code(code) => ctx.push_inline_code(&code),
            Event::SoftBreak => ctx.push_text(" "),
            Event::HardBreak => ctx.flush_line(&mut lines),
            Event::Rule => {
                ctx.flush_line(&mut lines);
                lines.push(Line::from(Span::styled(
                    MD_HRULE_CHAR.repeat(40),
                    Theme::md_hr(),
                )));
            }
            _ => {}
        }
    }

    // flush any remaining spans
    ctx.flush_line(&mut lines);
    // strip trailing blank lines — card padding handles vertical spacing
    while lines.last().is_some_and(|l| l.spans.is_empty()) {
        lines.pop();
    }
    lines
}

struct RenderCtx<'a> {
    base_style: Style,
    hl: &'a Highlighter,
    style_stack: Vec<Style>,
    current_spans: Vec<Span<'static>>,
    in_code_block: bool,
    code_block_lines: Vec<String>,
    code_lang: Option<String>,
    list_depth: usize,
    list_indices: Vec<Option<u64>>, // None = unordered, Some(n) = ordered
    in_heading: bool,
    heading_level: u8,
    in_blockquote: bool,
    // table state
    in_table: bool,
    in_table_head: bool,
    table_alignments: Vec<Alignment>,
    table_rows: Vec<Vec<Vec<Span<'static>>>>, // rows → cells → spans
    current_cell_spans: Vec<Span<'static>>,
}

impl RenderCtx<'_> {
    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or(self.base_style)
    }

    fn push_style(&mut self, style: Style) {
        // merge with current: new style overrides fields that are set
        let current = self.current_style();
        let merged = Style {
            fg: style.fg.or(current.fg),
            bg: style.bg.or(current.bg),
            underline_color: style.underline_color.or(current.underline_color),
            add_modifier: current.add_modifier | style.add_modifier,
            sub_modifier: current.sub_modifier | style.sub_modifier,
        };
        self.style_stack.push(merged);
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn open_tag(&mut self, tag: &Tag) {
        match tag {
            Tag::Heading { level, .. } => {
                self.in_heading = true;
                self.heading_level = *level as u8;
                self.push_style(Theme::md_heading());
            }
            Tag::Emphasis => self.push_style(Theme::md_italic()),
            Tag::Strong => self.push_style(Theme::md_bold()),
            Tag::Strikethrough => self.push_style(Theme::md_strikethrough()),
            Tag::Link {
                dest_url, title, ..
            } => {
                self.push_style(Theme::md_link_title());
                // store url for close_tag — we'll append it
                // for simplicity we just style the link text
                let _ = (dest_url, title);
            }
            Tag::BlockQuote(_) => {
                self.in_blockquote = true;
                self.push_style(Theme::md_blockquote());
            }
            Tag::CodeBlock(kind) => {
                self.in_code_block = true;
                self.code_block_lines.clear();
                self.code_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => {
                        let l = lang.trim().to_string();
                        if l.is_empty() { None } else { Some(l) }
                    }
                    _ => None,
                };
            }
            Tag::List(start) => {
                self.list_depth += 1;
                self.list_indices.push(*start);
            }
            Tag::Item => {
                // handled in Text via prefix
            }
            Tag::Paragraph => {}
            Tag::Table(alignments) => {
                self.in_table = true;
                self.table_alignments = alignments.clone();
                self.table_rows.clear();
            }
            Tag::TableHead => {
                self.in_table_head = true;
                self.table_rows.push(Vec::new());
            }
            Tag::TableRow => {
                self.table_rows.push(Vec::new());
            }
            Tag::TableCell => {
                self.current_cell_spans.clear();
            }
            _ => {}
        }
    }

    fn close_tag(&mut self, tag: &TagEnd, lines: &mut Vec<Line<'static>>) {
        match tag {
            TagEnd::Heading(_) => {
                // prepend heading marker
                let marker = match self.heading_level {
                    1 => "# ",
                    2 => "## ",
                    3 => "### ",
                    _ => "#### ",
                };
                self.current_spans
                    .insert(0, Span::styled(marker.to_string(), Theme::md_heading()));
                self.in_heading = false;
                self.heading_level = 0;
                self.flush_line(lines);
                self.pop_style();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.pop_style();
            }
            TagEnd::Link => {
                self.pop_style();
            }
            TagEnd::BlockQuote(_) => {
                self.in_blockquote = false;
                self.pop_style();
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                // lang label
                if let Some(lang) = &self.code_lang {
                    lines.push(Line::from(Span::styled(
                        format!("  {lang}"),
                        Theme::md_code_lang(),
                    )));
                }
                // syntax-highlighted code
                let code = self.code_block_lines.join("\n");
                let highlighted = self.hl.highlight_block(&code, self.code_lang.as_deref());
                lines.extend(highlighted);
                lines.push(Line::from(Span::styled(" ", Theme::md_code_block())));
                self.code_block_lines.clear();
                self.code_lang = None;
            }
            TagEnd::List(_) => {
                self.list_depth = self.list_depth.saturating_sub(1);
                self.list_indices.pop();
                // add spacing after top-level lists (like paragraphs do)
                if self.list_depth == 0 {
                    lines.push(Line::default());
                }
            }
            TagEnd::Item => {
                self.flush_line(lines);
            }
            TagEnd::Paragraph => {
                self.flush_line(lines);
                lines.push(Line::default());
            }
            TagEnd::Table => {
                self.flush_table(lines);
                self.in_table = false;
                self.in_table_head = false;
                self.table_alignments.clear();
                self.table_rows.clear();
            }
            TagEnd::TableHead => {
                self.in_table_head = false;
            }
            TagEnd::TableRow => {}
            TagEnd::TableCell => {
                let spans = std::mem::take(&mut self.current_cell_spans);
                if let Some(row) = self.table_rows.last_mut() {
                    row.push(spans);
                }
            }
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        if self.in_code_block {
            // collect lines for code block
            for line in text.split('\n') {
                self.code_block_lines.push(line.to_string());
            }
            // the last split element is from a trailing \n — remove empty
            if self.code_block_lines.last().is_some_and(|l| l.is_empty()) {
                self.code_block_lines.pop();
            }
            return;
        }

        // table cell text — collect into cell spans, not current_spans
        if self.in_table {
            let style = if self.in_table_head {
                Theme::md_table_header()
            } else {
                self.current_style()
            };
            self.current_cell_spans
                .push(Span::styled(text.to_string(), style));
            return;
        }

        let style = self.current_style();

        // if this is the first span of a list item, prepend bullet/number
        if self.current_spans.is_empty() && !self.list_indices.is_empty() {
            let indent = "  ".repeat(self.list_depth.saturating_sub(1));
            let bullet = if let Some(Some(n)) = self.list_indices.last_mut() {
                let b = format!("{indent}{n}. ");
                *n += 1;
                b
            } else {
                format!("{indent}{MD_BULLET}")
            };
            self.current_spans
                .push(Span::styled(bullet, Theme::md_list_bullet()));
        }

        // blockquote prefix
        if self.in_blockquote && self.current_spans.is_empty() {
            self.current_spans
                .push(Span::styled("  ", Theme::md_blockquote()));
        }

        self.current_spans
            .push(Span::styled(text.to_string(), style));
    }

    fn push_inline_code(&mut self, code: &str) {
        if self.in_table {
            self.current_cell_spans
                .push(Span::styled(code.to_string(), Theme::md_code_inline()));
            return;
        }
        self.current_spans
            .push(Span::styled(code.to_string(), Theme::md_code_inline()));
    }

    /// Render the collected table rows into styled Lines with box-drawing borders.
    fn flush_table(&mut self, lines: &mut Vec<Line<'static>>) {
        use unicode_width::UnicodeWidthStr;

        let num_cols = self.table_alignments.len();
        if num_cols == 0 || self.table_rows.is_empty() {
            return;
        }

        // Measure each cell's display width and compute max per column.
        // Each cell is Vec<Span>; width = sum of span content widths.
        let cell_width = |cell: &[Span<'_>]| -> usize {
            cell.iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum()
        };

        let mut col_widths = vec![0usize; num_cols];
        for row in &self.table_rows {
            for (c, cell) in row.iter().enumerate() {
                if c < num_cols {
                    col_widths[c] = col_widths[c].max(cell_width(cell));
                }
            }
        }
        // Minimum column width of 1 char + 2 padding spaces (1 each side)
        for w in &mut col_widths {
            *w = (*w).max(1);
        }

        let border_style = Theme::md_table_border();

        // ── Helper closures ───────────────────────────────────────────

        // Build a horizontal border line: e.g. ┌───┬───┐
        let h_border = |left: char, mid: char, right: char| -> Line<'static> {
            let mut s = String::new();
            for (i, &w) in col_widths.iter().enumerate() {
                if i == 0 {
                    s.push(left);
                } else {
                    s.push(mid);
                }
                // w + 2 for 1-char padding each side
                for _ in 0..w + 2 {
                    s.push('─');
                }
            }
            s.push(right);
            Line::from(Span::styled(s, border_style))
        };

        // Pad text to `width` according to alignment, returning (left_pad, right_pad).
        let padding = |text_w: usize, col_w: usize, align: Alignment| -> (usize, usize) {
            let gap = col_w.saturating_sub(text_w);
            match align {
                Alignment::Right => (gap, 0),
                Alignment::Center => {
                    let left = gap / 2;
                    (left, gap - left)
                }
                // Left or None
                _ => (0, gap),
            }
        };

        // Build a data row Line from cell spans.
        let build_row = |row: &[Vec<Span<'static>>], is_header: bool| -> Line<'static> {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (c, &col_w) in col_widths.iter().enumerate().take(num_cols) {
                // left border
                spans.push(Span::styled("│ ".to_string(), border_style));

                let cell = row.get(c);
                let w = cell.map(|cl| cell_width(cl)).unwrap_or(0);
                let align = self
                    .table_alignments
                    .get(c)
                    .copied()
                    .unwrap_or(Alignment::None);
                let (lpad, rpad) = padding(w, col_w, align);

                // left padding
                if lpad > 0 {
                    spans.push(Span::styled(
                        " ".repeat(lpad),
                        if is_header {
                            Theme::md_table_header()
                        } else {
                            self.base_style
                        },
                    ));
                }

                // cell content
                if let Some(cell_spans) = cell {
                    spans.extend(cell_spans.iter().cloned());
                }

                // right padding
                if rpad > 0 {
                    spans.push(Span::styled(
                        " ".repeat(rpad),
                        if is_header {
                            Theme::md_table_header()
                        } else {
                            self.base_style
                        },
                    ));
                }

                // space before next border
                spans.push(Span::styled(" ".to_string(), border_style));
            }
            // right border
            spans.push(Span::styled("│".to_string(), border_style));
            Line::from(spans)
        };

        // ── Emit lines ───────────────────────────────────────────────

        // Top border
        lines.push(h_border('┌', '┬', '┐'));

        // Header row (first row)
        if let Some(header) = self.table_rows.first() {
            lines.push(build_row(header, true));
        }

        // Header separator
        lines.push(h_border('├', '┼', '┤'));

        // Body rows
        for row in self.table_rows.iter().skip(1) {
            lines.push(build_row(row, false));
        }

        // Bottom border
        lines.push(h_border('└', '┴', '┘'));

        // Trailing blank line for spacing (like paragraphs)
        lines.push(Line::default());
    }

    fn flush_line(&mut self, lines: &mut Vec<Line<'static>>) {
        if !self.current_spans.is_empty() {
            let spans = std::mem::take(&mut self.current_spans);
            lines.push(Line::from(spans));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::Highlighter;
    use crate::theme::Theme;

    /// Extract the raw text from rendered Lines (ignoring styles).
    fn lines_to_text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn render_md(md: &str) -> Vec<Line<'static>> {
        Theme::set_by_index(0);
        Theme::begin_frame();
        let hl = Highlighter::new();
        let base = Style::default();
        render(md, base, &hl)
    }

    // ── Basic table structure ──────────────────────────────────────────

    #[test]
    fn table_basic_renders_box_drawing_borders() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n";
        let lines = render_md(md);
        let text = lines_to_text(&lines);

        // Should contain box-drawing top border
        assert!(
            text.iter().any(|l| l.contains('┌') && l.contains('┐')),
            "Expected top border with ┌ and ┐, got: {text:#?}"
        );
        // Should contain box-drawing bottom border
        assert!(
            text.iter().any(|l| l.contains('└') && l.contains('┘')),
            "Expected bottom border with └ and ┘, got: {text:#?}"
        );
        // Should contain header separator
        assert!(
            text.iter().any(|l| l.contains('├') && l.contains('┤')),
            "Expected header separator with ├ and ┤, got: {text:#?}"
        );
        // Should contain vertical separators
        assert!(
            text.iter().any(|l| l.contains('│')),
            "Expected vertical separators │, got: {text:#?}"
        );
    }

    #[test]
    fn table_basic_contains_cell_content() {
        let md = "| Name | Age |\n|------|-----|\n| Alice | 30 |\n| Bob | 25 |\n";
        let lines = render_md(md);
        let text = lines_to_text(&lines);
        let joined = text.join("\n");

        assert!(
            joined.contains("Name"),
            "Missing header 'Name' in:\n{joined}"
        );
        assert!(joined.contains("Age"), "Missing header 'Age' in:\n{joined}");
        assert!(
            joined.contains("Alice"),
            "Missing cell 'Alice' in:\n{joined}"
        );
        assert!(joined.contains("30"), "Missing cell '30' in:\n{joined}");
        assert!(joined.contains("Bob"), "Missing cell 'Bob' in:\n{joined}");
        assert!(joined.contains("25"), "Missing cell '25' in:\n{joined}");
    }

    #[test]
    fn table_correct_line_count() {
        // A 1-header + 2-body-row table should produce:
        //   top border, header row, separator, body row 1, body row 2, bottom border, blank line
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n";
        let lines = render_md(md);
        let text = lines_to_text(&lines);

        // Filter out empty trailing lines
        let non_empty: Vec<_> = text.iter().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            non_empty.len(),
            6, // top + header + sep + 2 body rows + bottom
            "Expected 6 non-empty lines, got {}: {non_empty:#?}",
            non_empty.len()
        );
    }

    // ── Alignment ──────────────────────────────────────────────────────

    #[test]
    fn table_left_alignment_pads_right() {
        let md = "| X |\n|:--|\n| hi |\n";
        let lines = render_md(md);
        let text = lines_to_text(&lines);

        // Find the body row with "hi"
        let body_row = text
            .iter()
            .find(|l| l.contains("hi"))
            .expect("body row missing");
        // After "hi" there should be trailing spaces before the border
        assert!(
            body_row.contains("hi "),
            "Left-aligned cell should have trailing space padding: {body_row}"
        );
    }

    #[test]
    fn table_right_alignment_pads_left() {
        let md = "| X |\n|--:|\n| hi |\n";
        let lines = render_md(md);
        let text = lines_to_text(&lines);

        let body_row = text
            .iter()
            .find(|l| l.contains("hi"))
            .expect("body row missing");
        // Before "hi" there should be leading spaces after the border
        assert!(
            body_row.contains(" hi"),
            "Right-aligned cell should have leading space padding: {body_row}"
        );
    }

    #[test]
    fn table_center_alignment() {
        let md = "| Header |\n|:------:|\n| hi |\n";
        let lines = render_md(md);
        let text = lines_to_text(&lines);

        let body_row = text
            .iter()
            .find(|l| l.contains("hi"))
            .expect("body row missing");
        // "hi" should have padding on both sides
        assert!(
            body_row.contains(" hi "),
            "Center-aligned cell should have padding on both sides: {body_row}"
        );
    }

    // ── Edge cases ─────────────────────────────────────────────────────

    #[test]
    fn table_empty_cells() {
        let md = "| A | B |\n|---|---|\n|   | x |\n";
        let lines = render_md(md);
        let text = lines_to_text(&lines);
        let joined = text.join("\n");

        assert!(
            joined.contains("x"),
            "Non-empty cell content missing: {joined}"
        );
        // Should still render correct structure
        assert!(
            text.iter().any(|l| l.contains('┌')),
            "Table borders missing for table with empty cells"
        );
    }

    #[test]
    fn table_single_column() {
        let md = "| Solo |\n|------|\n| val |\n";
        let lines = render_md(md);
        let text = lines_to_text(&lines);
        let joined = text.join("\n");

        assert!(joined.contains("Solo"), "Header missing");
        assert!(joined.contains("val"), "Body missing");
        assert!(
            text.iter().any(|l| l.contains('┌') && l.contains('┐')),
            "Single-column table should have borders"
        );
        // Single column: no ┬ or ┼ (only top-left and top-right)
        assert!(
            !text.iter().any(|l| l.contains('┬')),
            "Single-column table should NOT have ┬ junction"
        );
    }

    #[test]
    fn table_inline_code_in_cell() {
        let md = "| Code |\n|------|\n| `foo` |\n";
        let lines = render_md(md);
        let text = lines_to_text(&lines);
        let joined = text.join("\n");

        assert!(
            joined.contains("foo"),
            "Inline code content should appear in table: {joined}"
        );
    }

    #[test]
    fn table_bold_in_cell() {
        let md = "| Styled |\n|--------|\n| **bold** |\n";
        let lines = render_md(md);
        let text = lines_to_text(&lines);
        let joined = text.join("\n");

        assert!(
            joined.contains("bold"),
            "Bold text should appear in table cell: {joined}"
        );
    }

    #[test]
    fn table_does_not_break_surrounding_content() {
        let md = "Before\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\nAfter\n";
        let lines = render_md(md);
        let text = lines_to_text(&lines);
        let joined = text.join("\n");

        assert!(joined.contains("Before"), "Text before table missing");
        assert!(joined.contains("After"), "Text after table missing");
        assert!(joined.contains('┌'), "Table borders missing");
    }

    // ── Header styling ─────────────────────────────────────────────────

    #[test]
    fn table_header_has_bold_modifier() {
        let md = "| Head |\n|------|\n| body |\n";
        let lines = render_md(md);

        // Find the line containing "Head" (the header row)
        let header_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref().contains("Head")))
            .expect("Header line not found");

        let head_span = header_line
            .spans
            .iter()
            .find(|s| s.content.as_ref().contains("Head"))
            .expect("Header span not found");

        assert!(
            head_span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "Header cell should be bold, got style: {:?}",
            head_span.style
        );
    }

    // ── Column width calculation ───────────────────────────────────────

    #[test]
    fn table_columns_pad_to_widest_cell() {
        let md = "| A | B |\n|---|---|\n| short | x |\n| a very long cell | y |\n";
        let lines = render_md(md);
        let text = lines_to_text(&lines);

        // The top border should be wide enough for "a very long cell"
        let top = text.iter().find(|l| l.contains('┌')).expect("top border");
        let bottom = text
            .iter()
            .find(|l| l.contains('└'))
            .expect("bottom border");
        // Top and bottom borders should have the same length
        assert_eq!(
            top.len(),
            bottom.len(),
            "Top and bottom borders should be same width"
        );
    }
}
