use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};

use crate::app::App;
use crate::theme::Theme;

use super::{ELLIPSIS, draw_header, relative_time};

// ── Start-page session list ────────────────────────────────────────────────────

pub(super) const COLLAPSE_OPEN: &str = "\u{25BE}"; // ▾ expanded group
pub(super) const COLLAPSE_CLOSED: &str = "\u{25B8}"; // ▸ collapsed group

/// A single rendered row for the start-page session list.
///
/// Returned by [`build_start_page_rows`] and consumed by [`draw_start`].
pub(crate) struct StartPageRow {
    /// The logical item this row represents.
    pub(crate) item: crate::app::StartPageItem,
    /// Pre-rendered line (spans already styled).
    pub(crate) line: Line<'static>,
    /// True when `session_cursor` points at this row.
    pub(crate) selected: bool,
}

/// Build the displayable rows for the start-page session list.
///
/// Pure function — does not touch the frame. Accepts `area_width` for
/// truncating long paths/titles.
pub(crate) fn build_start_page_rows(app: &App, area_width: usize) -> Vec<StartPageRow> {
    let items = app.visible_start_items();
    let mut rows = Vec::with_capacity(items.len());

    for (idx, item) in items.iter().enumerate() {
        let selected = idx == app.session_cursor;

        let (header_style, dim_style) = if selected {
            (Theme::selected(), Theme::selected())
        } else {
            (Theme::start_header(), Theme::start_dim())
        };
        let session_style = if selected {
            Theme::selected()
        } else {
            Theme::start_session()
        };

        let line: Line<'static> = match &item {
            crate::app::StartPageItem::GroupHeader {
                cwd,
                session_count,
                collapsed,
            } => {
                let indicator = if *collapsed {
                    COLLAPSE_CLOSED
                } else {
                    COLLAPSE_OPEN
                };
                let cwd_display = cwd.as_deref().unwrap_or("(no workspace)");
                // Shorten very long paths: keep last 3 components
                let cwd_short = short_cwd(cwd_display, area_width.saturating_sub(16));
                Line::from(vec![
                    Span::styled(format!(" {indicator} "), header_style),
                    Span::styled(cwd_short, header_style),
                    Span::styled(format!("  ({session_count}) "), dim_style),
                ])
            }

            crate::app::StartPageItem::Session {
                group_idx,
                session_idx,
            } => {
                let session = &app.session_groups[*group_idx].sessions[*session_idx];
                let id_short: String = session.session_id.chars().take(8).collect();
                let title = session.title.as_deref().unwrap_or("(untitled)");
                let time_str = session
                    .updated_at
                    .as_deref()
                    .map(relative_time)
                    .unwrap_or_default();

                // Budget: "   {id_short}  " + time_str = fixed overhead ~20 chars
                let overhead = 3 + 8 + 2 + time_str.len() + 2;
                let avail = area_width.saturating_sub(overhead);
                let title_display: String = if title.chars().count() > avail && avail > 1 {
                    let t: String = title.chars().take(avail.saturating_sub(1)).collect();
                    format!("{t}{ELLIPSIS}")
                } else {
                    title.to_string()
                };
                let gap = avail.saturating_sub(title_display.chars().count());

                Line::from(vec![
                    Span::styled(format!("   {id_short}  "), dim_style),
                    Span::styled(title_display, session_style),
                    Span::styled(" ".repeat(gap), session_style),
                    Span::styled(format!("  {time_str} "), dim_style),
                ])
            }

            crate::app::StartPageItem::ShowMore { remaining, .. } => {
                let total = remaining + crate::app::MAX_RECENT_SESSIONS;
                let label = format!("   \u{2026}  show all ({total} total)");
                Line::from(vec![Span::styled(label, dim_style)])
            }
        };

        rows.push(StartPageRow {
            item: item.clone(),
            line,
            selected,
        });
    }

    rows
}

/// Shorten a path to fit within `max_chars` by keeping the last N components.
pub(super) fn short_cwd(path: &str, max_chars: usize) -> String {
    if path.chars().count() <= max_chars {
        return path.to_string();
    }
    // Walk backwards collecting components until we exceed max_chars
    let mut parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    while !parts.is_empty() {
        let candidate = format!("\u{2026}/{}", parts.join("/"));
        if candidate.chars().count() <= max_chars {
            return candidate;
        }
        parts.remove(0);
    }
    // Absolute fallback: hard-truncate
    let t: String = path.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{t}{ELLIPSIS}")
}

pub(super) fn draw_start(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Minimum height to show the ASCII art: art (6 rows) + 2 padding = 8.
    // Below this threshold we skip the art and give all space to the list.
    const ART_MIN_HEIGHT: u16 = 16;
    const ART_ROWS: u16 = 6;

    let show_art = area.height >= ART_MIN_HEIGHT;
    let art_section_height = if show_art { ART_ROWS + 2 } else { 0 };

    // ── outer layout: header │ middle │ hints ─────────────────────────────────
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(1),    // middle (centred content)
            Constraint::Length(1), // hints
        ])
        .split(area);

    // ── header ────────────────────────────────────────────────────────────────
    draw_header(
        f,
        app,
        outer[0],
        vec![Span::styled(format!(" {}", app.status), Theme::status())],
        vec![],
    );

    // ── hints ─────────────────────────────────────────────────────────────────
    // Rendered now (fixed bottom) so we can focus the rest on centring.
    let hint_area = outer[2];
    let hints = Line::from(vec![
        Span::styled(" \u{2191}\u{2193} ", Theme::status_accent()),
        Span::styled("navigate  ", Theme::status()),
        Span::styled("enter ", Theme::status_accent()),
        Span::styled("open/collapse  ", Theme::status()),
        Span::styled("del ", Theme::status_accent()),
        Span::styled("delete  ", Theme::status()),
        Span::styled("C-x n ", Theme::status_accent()),
        Span::styled("new  ", Theme::status()),
        Span::styled("C-x s ", Theme::status_accent()),
        Span::styled("popup  ", Theme::status()),
        Span::styled("C-x m ", Theme::status_accent()),
        Span::styled("model  ", Theme::status()),
        Span::styled("q ", Theme::status_accent()),
        Span::styled("quit", Theme::status()),
    ]);
    f.render_widget(Paragraph::new(hints).style(Theme::base()), hint_area);

    // ── glitch / wave variables (shared by art and button) ───────────────────
    const GLITCH_CHARS: &str = "░▒▓█▌▐▄▀┃╋╳";
    let tick = app.tick as usize;
    let prng = |seed: usize| -> usize {
        let mut h = seed.wrapping_mul(2654435761);
        h ^= h >> 16;
        h.wrapping_mul(0x45d9f3b) ^ (h >> 13)
    };
    let wave_colors = [
        Theme::accent(),
        Theme::info(),
        Theme::ok(),
        Theme::warn(),
        Theme::accent(),
    ];
    let glitch_frame = prng(tick / 3) % 7 == 0;

    // ── content column: fixed 64-char wide, centred under the ASCII art ───────
    const CONTENT_COL_W: u16 = 64;
    let col_w = CONTENT_COL_W.min(area.width);
    let col_x = area.x + area.width.saturating_sub(col_w) / 2;

    // ── compute content block height for vertical centring ────────────────────
    // Build rows first so we know how many there are.
    let rows = build_start_page_rows(app, col_w as usize);
    let rows_h = rows.len() as u16;

    // Button: 1 gap row + 1 text row (no border).
    const BUTTON_H: u16 = 2; // 1 gap + 1 button

    // Total content = art section + filter row + gap + session rows + button.
    // Cap rows_h to what fits so centring stays correct on short terminals.
    let middle = outer[1];
    let max_rows_h = middle
        .height
        .saturating_sub(art_section_height + 1 + 1 + BUTTON_H);
    let capped_rows_h = rows_h.min(max_rows_h);
    let content_h = (art_section_height + 1 + 1 + capped_rows_h + BUTTON_H).min(middle.height);

    // Centre the content block vertically inside `middle`.
    let top_pad = middle.height.saturating_sub(content_h) / 2;
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top_pad),   // top spacer
            Constraint::Length(content_h), // content block
            Constraint::Min(0),            // bottom spacer
        ])
        .split(middle);

    let content_area = inner[1];

    // Sub-divide content_area into art | filter | gap | session rows | gap | button.
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(art_section_height), // [0] art
            Constraint::Length(1),                  // [1] filter
            Constraint::Length(1),                  // [2] gap after filter
            Constraint::Min(0),                     // [3] session rows
            Constraint::Length(1),                  // [4] gap before button
            Constraint::Length(1),                  // [5] button
        ])
        .split(content_area);

    // ── ASCII art ─────────────────────────────────────────────────────────────
    if show_art {
        let art_area = sections[0];
        let art = [
            r"  ___                        __  __ _____ ",
            r" / _ \ _   _  ___ _ __ _   _|  \/  |_   _|",
            r"| | | | | | |/ _ \ '__| | | | |\/| | | |  ",
            r"| |_| | |_| |  __/ |  | |_| | |  | | | |  ",
            r" \__\_\\__,_|\___|_|   \__, |_|  |_| |_|  ",
            r"                       |___/               ",
        ];
        let art_w = art.iter().map(|l| l.len()).max().unwrap_or(0) as u16;
        // Centre vertically within the art section (1 row padding top)
        let start_y = art_area.y + 1;
        let start_x = art_area.x + art_area.width.saturating_sub(art_w) / 2;

        let glitch_row = if glitch_frame {
            Some(prng(tick / 3 + 99) % art.len())
        } else {
            None
        };

        for (row, line) in art.iter().enumerate() {
            if start_y + row as u16 >= art_area.y + art_area.height {
                break;
            }
            let x_offset = if glitch_row == Some(row) {
                (prng(tick + row * 7) % 5) as i16 - 2
            } else {
                0
            };
            let row_x = (start_x as i16 + x_offset).max(art_area.x as i16) as u16;

            let spans: Vec<Span<'static>> = line
                .chars()
                .enumerate()
                .map(|(col, ch)| {
                    if ch == ' ' {
                        return Span::raw(" ");
                    }
                    let char_glitch =
                        glitch_frame && prng(tick.wrapping_mul(col + 1) + row * 31) % 12 == 0;
                    let display_ch = if char_glitch {
                        let idx = prng(tick + col * 13 + row * 7) % GLITCH_CHARS.chars().count();
                        GLITCH_CHARS.chars().nth(idx).unwrap_or(ch)
                    } else {
                        ch
                    };
                    let color = if char_glitch {
                        Theme::err()
                    } else {
                        let phase = (col + row * 3).wrapping_add(tick / 3);
                        wave_colors[phase % wave_colors.len()]
                    };
                    Span::styled(
                        display_ch.to_string(),
                        ratatui::style::Style::default()
                            .fg(color)
                            .add_modifier(ratatui::style::Modifier::BOLD),
                    )
                })
                .collect();
            f.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect {
                    x: row_x,
                    y: start_y + row as u16,
                    width: art_w.min(art_area.width),
                    height: 1,
                },
            );
        }
    }

    // ── filter input ──────────────────────────────────────────────────────────
    let filter_area = Rect {
        x: col_x,
        y: sections[1].y,
        width: col_w,
        height: 1,
    };
    let filter_line = Line::from(vec![
        Span::styled(" > ", Theme::start_header()),
        Span::styled(app.session_filter.clone(), Theme::fg()),
    ]);
    f.render_widget(
        Paragraph::new(filter_line).style(Theme::base()),
        filter_area,
    );
    // Show cursor at the end of the typed filter text
    f.set_cursor_position((
        filter_area.x + 3 + app.session_filter.chars().count() as u16,
        filter_area.y,
    ));

    // ── session list ──────────────────────────────────────────────────────────
    let list_area = sections[3];

    if rows.is_empty() {
        // Empty state
        let msg = if app.session_filter.is_empty() {
            "No sessions yet.  C-x n to start a new one."
        } else {
            "No sessions match the filter."
        };
        let msg_w = msg.len() as u16;
        let msg_x = col_x + col_w.saturating_sub(msg_w) / 2;
        let msg_y = list_area.y + list_area.height / 2;
        f.render_widget(
            Paragraph::new(Span::styled(msg, Theme::status())),
            Rect {
                x: msg_x,
                y: msg_y,
                width: msg_w.min(col_w),
                height: 1,
            },
        );
    } else {
        // Scroll: keep the selected row visible.
        let visible_rows = list_area.height as usize;
        let total_rows = rows.len();

        // Clamp scroll so cursor is in view.
        if app.session_cursor >= app.start_page_scroll + visible_rows {
            app.start_page_scroll = app.session_cursor + 1 - visible_rows;
        }
        if app.session_cursor < app.start_page_scroll {
            app.start_page_scroll = app.session_cursor;
        }
        app.start_page_scroll = app
            .start_page_scroll
            .min(total_rows.saturating_sub(visible_rows));

        for (display_row, row) in rows
            .iter()
            .skip(app.start_page_scroll)
            .take(visible_rows)
            .enumerate()
        {
            let y = list_area.y + display_row as u16;
            if y >= list_area.y + list_area.height {
                break;
            }
            let row_area = Rect {
                x: col_x,
                y,
                width: col_w,
                height: 1,
            };

            // Fill background for the entire row first
            let row_bg = if row.selected {
                Theme::selected()
            } else {
                Theme::base()
            };
            f.render_widget(Block::default().style(row_bg), row_area);
            f.render_widget(Paragraph::new(row.line.clone()), row_area);
        }
    }

    // ── New Session button ────────────────────────────────────────────────────
    const BUTTON_TEXT: &str = "+ New Session";
    let button_w = (BUTTON_TEXT.len() as u16).min(col_w);
    let button_x = col_x + col_w.saturating_sub(button_w) / 2;
    let button_rect = Rect {
        x: button_x,
        y: sections[5].y,
        width: button_w,
        height: 1,
    };

    let button_focused = app.session_cursor == rows.len();

    // Build text spans: glitch only when focused, always bold
    let text_spans: Vec<Span<'static>> = if button_focused {
        BUTTON_TEXT
            .chars()
            .enumerate()
            .map(|(col, ch)| {
                if ch == ' ' {
                    return Span::raw(" ");
                }
                let char_glitch = glitch_frame && prng(tick.wrapping_mul(col + 1) + 999) % 8 == 0;
                let display_ch = if char_glitch {
                    let idx = prng(tick + col * 17 + 333) % GLITCH_CHARS.chars().count();
                    GLITCH_CHARS.chars().nth(idx).unwrap_or(ch)
                } else {
                    ch
                };
                let color = if char_glitch {
                    Theme::err()
                } else {
                    let phase = (col + tick / 3) % wave_colors.len();
                    wave_colors[phase]
                };
                Span::styled(
                    display_ch.to_string(),
                    ratatui::style::Style::default()
                        .fg(color)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                )
            })
            .collect()
    } else {
        vec![Span::styled(
            BUTTON_TEXT,
            Theme::start_header().add_modifier(ratatui::style::Modifier::BOLD),
        )]
    };

    f.render_widget(
        Paragraph::new(Line::from(text_spans)).alignment(Alignment::Center),
        button_rect,
    );
}
