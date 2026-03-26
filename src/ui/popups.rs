use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph},
};

use crate::app::App;
use crate::theme::Theme;

use super::{
    ARROW_DOWN, ARROW_UP, COLLAPSE_CLOSED, COLLAPSE_OPEN, COLOR_SWATCH, ELLIPSIS, relative_time,
    short_cwd,
};

// ── Centered rect helper ──────────────────────────────────────────────────────

pub(crate) fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

// ── Model popup ───────────────────────────────────────────────────────────────

pub(super) fn draw_model_popup(f: &mut Frame, app: &App) {
    let area = f.area();
    let popup_area = centered_rect(60, 60, area);

    f.render_widget(Clear, popup_area);
    f.render_widget(Block::default().style(Theme::popup_bg()), popup_area);

    let inner = Rect {
        x: popup_area.x + 1,
        y: popup_area.y + 1,
        width: popup_area.width.saturating_sub(2),
        height: popup_area.height.saturating_sub(2),
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // filter
            Constraint::Length(1), // spacer
            Constraint::Min(1),    // list
            Constraint::Length(1), // hint
        ])
        .split(inner);

    // title
    f.render_widget(
        Paragraph::new(Span::styled("select model", Theme::popup_title())).style(Theme::popup_bg()),
        chunks[0],
    );

    // filter
    let filter_line = Line::from(vec![
        Span::styled("> ", Theme::popup_title()),
        Span::styled(app.model_filter.clone(), Theme::popup_bg()),
    ]);
    f.render_widget(
        Paragraph::new(filter_line).style(Theme::popup_bg()),
        chunks[1],
    );

    f.set_cursor_position((chunks[1].x + 2 + app.model_filter.len() as u16, chunks[1].y));

    // model list
    let filtered = app.filtered_models();
    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let (main_style, dim_style) = if i == app.model_cursor {
                (Theme::selected(), Theme::selected())
            } else {
                (Theme::popup_bg(), Theme::status())
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", m.provider), dim_style),
                Span::styled(m.label.clone(), main_style),
            ]))
        })
        .collect();

    let list = List::new(items).block(Block::default().style(Theme::popup_bg()));
    let mut state = ListState::default().with_selected(Some(app.model_cursor));
    f.render_stateful_widget(list, chunks[3], &mut state);

    // hint
    f.render_widget(
        Paragraph::new(Span::styled(" esc cancel  enter select", Theme::status()))
            .style(Theme::popup_bg()),
        chunks[4],
    );
}

// ── Session popup ─────────────────────────────────────────────────────────────

pub(super) fn draw_session_popup(f: &mut Frame, app: &App) {
    use crate::app::PopupItem;

    let area = f.area();
    let popup_area = centered_rect(70, 60, area);

    f.render_widget(Clear, popup_area);
    f.render_widget(Block::default().style(Theme::popup_bg()), popup_area);

    let inner = Rect {
        x: popup_area.x + 1,
        y: popup_area.y + 1,
        width: popup_area.width.saturating_sub(2),
        height: popup_area.height.saturating_sub(2),
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // filter
            Constraint::Length(1), // spacer
            Constraint::Min(1),    // list
            Constraint::Length(1), // hint
        ])
        .split(inner);

    // title
    f.render_widget(
        Paragraph::new(Span::styled("sessions", Theme::popup_title())).style(Theme::popup_bg()),
        chunks[0],
    );

    // filter
    let filter_line = Line::from(vec![
        Span::styled("> ", Theme::popup_title()),
        Span::styled(app.session_filter.clone(), Theme::popup_bg()),
    ]);
    f.render_widget(
        Paragraph::new(filter_line).style(Theme::popup_bg()),
        chunks[1],
    );
    f.set_cursor_position((
        chunks[1].x + 2 + app.session_filter.chars().count() as u16,
        chunks[1].y,
    ));

    // grouped session list
    let popup_items = app.visible_popup_items();
    let list_w = chunks[3].width as usize;

    let items: Vec<ListItem> = popup_items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let selected = i == app.session_cursor;
            match item {
                PopupItem::GroupHeader {
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
                    let cwd_short = short_cwd(cwd_display, list_w.saturating_sub(16));
                    let (header_style, dim_style) = if selected {
                        (Theme::selected(), Theme::selected())
                    } else {
                        (Theme::status_accent(), Theme::status())
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(format!(" {indicator} "), header_style),
                        Span::styled(cwd_short, header_style),
                        Span::styled(format!("  ({session_count}) "), dim_style),
                    ]))
                }
                PopupItem::Session {
                    group_idx,
                    session_idx,
                } => {
                    let s = &app.session_groups[*group_idx].sessions[*session_idx];
                    let id_short: String = s.session_id.chars().take(8).collect();
                    let time_str = s
                        .updated_at
                        .as_deref()
                        .map(relative_time)
                        .unwrap_or_default();
                    let title = s.title.as_deref().unwrap_or("(untitled)");

                    let is_active = app.session_id.as_deref() == Some(s.session_id.as_str());
                    let id_part = format!("   {id_short} ");
                    let active_part = if is_active { " active " } else { "" };
                    let time_part = format!(" {time_str} ");
                    let avail = list_w.saturating_sub(
                        id_part.chars().count()
                            + active_part.chars().count()
                            + time_part.chars().count(),
                    );
                    let title_display = if title.chars().count() > avail {
                        let t: String = title.chars().take(avail.saturating_sub(1)).collect();
                        format!("{t}{ELLIPSIS}")
                    } else {
                        title.to_string()
                    };
                    let title_gap = avail.saturating_sub(title_display.chars().count());

                    let (main_style, dim_style, time_style) = if selected {
                        (Theme::selected(), Theme::selected(), Theme::selected())
                    } else {
                        (Theme::popup_bg(), Theme::status(), Theme::session_time())
                    };
                    let active_style = if selected {
                        Theme::selected()
                    } else {
                        Theme::status_accent()
                    };

                    let mut spans = vec![
                        Span::styled(id_part, dim_style),
                        Span::styled(title_display, main_style),
                        Span::styled(" ".repeat(title_gap), dim_style),
                    ];
                    if is_active {
                        spans.push(Span::styled(active_part, active_style));
                    }
                    spans.push(Span::styled(time_part, time_style));

                    ListItem::new(Line::from(spans))
                }
            }
        })
        .collect();

    let list = List::new(items).block(Block::default().style(Theme::popup_bg()));
    let mut state = ListState::default().with_selected(Some(app.session_cursor));
    f.render_stateful_widget(list, chunks[3], &mut state);

    // hint
    f.render_widget(
        Paragraph::new(Span::styled(
            " esc cancel  enter load/collapse  del delete  ctrl-n new",
            Theme::status(),
        ))
        .style(Theme::popup_bg()),
        chunks[4],
    );
}

// ── Theme list item builder ───────────────────────────────────────────────────

/// Builds a single [`ListItem`] for the theme picker list.
///
/// Layout (mirrors session-popup column style):
/// ```text
/// [marker][label padded to avail][■■■■■■■■■■■■■■■■]
/// ```
/// * `marker`   – `"* "` when `orig_idx == current_idx`, otherwise `"  "`
/// * `label`    – theme display name, truncated with `…` if needed
/// * swatches   – 16 `■` chars, each coloured with its base16 slot colour
///
/// The row background comes from `row_bg` (selected = `bg_hl`, normal = `bg_dim`).
pub(crate) fn build_theme_list_item(
    t: &crate::themes_gen::Base16Palette,
    orig_idx: usize,
    current_idx: usize,
    list_w: usize,
    is_selected: bool,
) -> ListItem<'static> {
    const NUM_SWATCHES: usize = 16;
    // " " gap between label and swatches
    const GAP: usize = 1;

    let marker = if orig_idx == current_idx { "* " } else { "  " };
    let marker_w = marker.chars().count();
    let swatches_w = NUM_SWATCHES + GAP; // 16 ■ + 1 space

    // Styles ─────────────────────────────────────────────────────────────────
    let (main_style, dim_style, row_bg) = if is_selected {
        (Theme::selected(), Theme::selected(), Theme::bg_hl())
    } else {
        (Theme::popup_bg(), Theme::status(), Theme::bg_dim())
    };

    // Label truncation (same pattern as session title) ───────────────────────
    let avail = list_w.saturating_sub(marker_w + swatches_w);
    let label: String = t.label.chars().collect();
    let label_display = if label.chars().count() > avail {
        let t: String = label.chars().take(avail.saturating_sub(1)).collect();
        format!("{t}{ELLIPSIS}")
    } else {
        label.clone()
    };
    let label_gap = avail.saturating_sub(label_display.chars().count());

    // Build spans ─────────────────────────────────────────────────────────────
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(3 + NUM_SWATCHES + 1);
    spans.push(Span::styled(marker, dim_style));
    spans.push(Span::styled(label_display, main_style));
    spans.push(Span::styled(" ".repeat(label_gap + GAP), dim_style));

    // 16 colour swatches ──────────────────────────────────────────────────────
    for &c in &t.colors {
        let fg = crate::theme::u32_to_color(c);
        spans.push(Span::styled(
            COLOR_SWATCH,
            ratatui::style::Style::default().fg(fg).bg(row_bg),
        ));
    }

    ListItem::new(Line::from(spans))
}

// ── New session popup ─────────────────────────────────────────────────────────

pub(super) fn draw_new_session_popup(f: &mut Frame, app: &App) {
    let area = f.area();
    let show_completion = app
        .new_session_completion
        .as_ref()
        .map(|completion| !completion.results.is_empty())
        .unwrap_or(false);
    let popup_width = area.width.saturating_sub(4).clamp(24, 72);
    let popup_height = area
        .height
        .saturating_sub(4)
        .min(if show_completion { 10 } else { 6 })
        .max(4);
    let popup_area = Rect {
        x: area.x + area.width.saturating_sub(popup_width) / 2,
        y: area.y + area.height.saturating_sub(popup_height) / 2,
        width: popup_width,
        height: popup_height,
    };

    f.render_widget(Clear, popup_area);
    f.render_widget(Block::default().style(Theme::popup_bg()), popup_area);

    let inner = Rect {
        x: popup_area.x + 1,
        y: popup_area.y + 1,
        width: popup_area.width.saturating_sub(2),
        height: popup_area.height.saturating_sub(2),
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(Span::styled("new session", Theme::popup_title())).style(Theme::popup_bg()),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            "workspace path (empty = default cwd)",
            Theme::status(),
        ))
        .style(Theme::popup_bg()),
        chunks[1],
    );
    f.render_widget(
        Paragraph::new(format!("> {}", app.new_session_path)).style(Theme::popup_bg()),
        chunks[2],
    );
    f.set_cursor_position((chunks[2].x + 2 + app.new_session_cursor as u16, chunks[2].y));

    if let Some(completion) = &app.new_session_completion
        && !completion.results.is_empty()
    {
        let items: Vec<ListItem> = completion
            .results
            .iter()
            .map(|entry| {
                ListItem::new(Line::from(vec![Span::styled(
                    entry.path.clone(),
                    Theme::input(),
                )]))
            })
            .collect();
        let list = List::new(items)
            .block(Block::default().style(Theme::popup_bg()))
            .highlight_style(Theme::selected())
            .highlight_symbol("");
        let selected = Some(completion.selected_index).filter(|_| !completion.results.is_empty());
        let mut state = ListState::default().with_selected(selected);
        f.render_stateful_widget(list, chunks[3], &mut state);
    }

    f.render_widget(
        Paragraph::new(Span::styled(
            "tab complete  enter start  esc cancel",
            Theme::status(),
        ))
        .style(Theme::popup_bg()),
        chunks[4],
    );
}

// ── Theme popup ───────────────────────────────────────────────────────────────

pub(super) fn draw_theme_popup(f: &mut Frame, app: &App) {
    let area = f.area();
    let popup_area = centered_rect(70, 60, area);

    f.render_widget(Clear, popup_area);
    f.render_widget(Block::default().style(Theme::popup_bg()), popup_area);

    let inner = Rect {
        x: popup_area.x + 1,
        y: popup_area.y + 1,
        width: popup_area.width.saturating_sub(2),
        height: popup_area.height.saturating_sub(2),
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // filter
            Constraint::Length(1), // spacer
            Constraint::Min(1),    // list
            Constraint::Length(1), // hint
        ])
        .split(inner);

    // title
    f.render_widget(
        Paragraph::new(Span::styled("theme", Theme::popup_title())).style(Theme::popup_bg()),
        chunks[0],
    );

    // filter
    let filter_line = Line::from(vec![
        Span::styled("> ", Theme::popup_title()),
        Span::styled(app.theme_filter.clone(), Theme::popup_bg()),
    ]);
    f.render_widget(
        Paragraph::new(filter_line).style(Theme::popup_bg()),
        chunks[1],
    );
    f.set_cursor_position((
        chunks[1].x + 2 + app.theme_filter.chars().count() as u16,
        chunks[1].y,
    ));

    // theme list
    let all_themes = Theme::available_themes();
    let filter_lower = app.theme_filter.to_lowercase();
    let filtered: Vec<(usize, &crate::themes_gen::Base16Palette)> = all_themes
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            filter_lower.is_empty()
                || t.label.to_lowercase().contains(&filter_lower)
                || t.id.to_lowercase().contains(&filter_lower)
        })
        .collect();

    let current_idx = Theme::current_index();
    let list_w = chunks[3].width as usize;

    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .map(|(i, (orig_idx, t))| {
            build_theme_list_item(t, *orig_idx, current_idx, list_w, i == app.theme_cursor)
        })
        .collect();

    let list = List::new(items).block(Block::default().style(Theme::popup_bg()));
    let mut state = ListState::default().with_selected(Some(app.theme_cursor));
    f.render_stateful_widget(list, chunks[3], &mut state);

    // hint
    f.render_widget(
        Paragraph::new(Span::styled(" esc cancel  enter apply", Theme::status()))
            .style(Theme::popup_bg()),
        chunks[4],
    );
}

// ── Log popup ─────────────────────────────────────────────────────────────────

pub(super) fn draw_log_popup(f: &mut Frame, app: &App) {
    let area = f.area();
    let popup_area = centered_rect(80, 70, area);

    f.render_widget(Clear, popup_area);
    f.render_widget(Block::default().style(Theme::popup_bg()), popup_area);

    let inner = Rect {
        x: popup_area.x + 1,
        y: popup_area.y + 1,
        width: popup_area.width.saturating_sub(2),
        height: popup_area.height.saturating_sub(2),
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // filter
            Constraint::Length(1), // level
            Constraint::Min(1),    // list
            Constraint::Length(1), // hint
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(Span::styled("logs", Theme::popup_title())).style(Theme::popup_bg()),
        chunks[0],
    );

    let filter_line = Line::from(vec![
        Span::styled("> ", Theme::popup_title()),
        Span::styled(app.log_filter.clone(), Theme::popup_bg()),
    ]);
    f.render_widget(
        Paragraph::new(filter_line).style(Theme::popup_bg()),
        chunks[1],
    );
    f.set_cursor_position((
        chunks[1].x + 2 + app.log_filter.chars().count() as u16,
        chunks[1].y,
    ));

    f.render_widget(
        Paragraph::new(Span::styled(
            format!("level: {}+", app.log_level_filter.label()),
            Theme::status(),
        ))
        .style(Theme::popup_bg()),
        chunks[2],
    );

    let filtered = app.filtered_logs();
    let list_w = chunks[3].width as usize;
    let items: Vec<ListItem> = if filtered.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            " no log entries match current filter",
            Theme::status(),
        )))]
    } else {
        filtered
            .iter()
            .map(|entry| {
                let prefix = format!(
                    " {:>6}.{:01} {:<5} {:<10} ",
                    entry.elapsed.as_secs(),
                    entry.elapsed.subsec_millis() / 100,
                    entry.level.label(),
                    entry.target
                );
                let avail = list_w.saturating_sub(prefix.chars().count());
                let message = if entry.message.chars().count() > avail {
                    let truncated: String = entry
                        .message
                        .chars()
                        .take(avail.saturating_sub(1))
                        .collect();
                    format!("{truncated}{ELLIPSIS}")
                } else {
                    entry.message.clone()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(prefix, Theme::status()),
                    Span::styled(message, Theme::popup_bg()),
                ]))
            })
            .collect()
    };

    let list = List::new(items).block(Block::default().style(Theme::popup_bg()));
    let selected =
        Some(app.log_cursor.min(filtered.len().saturating_sub(1))).filter(|_| !filtered.is_empty());
    let mut state = ListState::default().with_selected(selected);
    f.render_stateful_widget(list, chunks[3], &mut state);

    f.render_widget(
        Paragraph::new(Span::styled(
            " esc close  tab level  type filter",
            Theme::status(),
        ))
        .style(Theme::popup_bg()),
        chunks[4],
    );
}

// ── Help popup ────────────────────────────────────────────────────────────────

/// One section in the keyboard-shortcut reference.
pub(crate) struct ShortcutSection {
    pub title: &'static str,
    pub rows: &'static [(&'static str, &'static str)],
}

/// All shortcut sections shown in the help popup.
/// Keep entries sorted logically (not alphabetically).
pub(crate) fn shortcut_sections() -> &'static [ShortcutSection] {
    &[
        ShortcutSection {
            title: "global",
            rows: &[
                ("C-x \u{2026}", "chord prefix"),
                ("Tab", "cycle mode (build \u{2192} plan \u{2192} review)"),
                ("C-c", "clear input / quit"),
            ],
        },
        ShortcutSection {
            title: "chord  (C-x \u{2026})",
            rows: &[
                ("?", "this help"),
                ("e", "external editor"),
                ("m", "model selector"),
                ("n", "new session"),
                ("l", "logs popup"),
                ("q", "quit"),
                ("r", "redo"),
                ("s", "session switcher"),
                ("t", "theme picker"),
                ("u", "undo"),
            ],
        },
        ShortcutSection {
            title: "chat",
            rows: &[
                ("Enter", "send message"),
                ("Esc", "cancel / dismiss mention"),
                ("\u{2191} \u{2193}", "scroll history / navigate mentions"),
                ("PgUp PgDn", "scroll fast"),
                ("\u{2190} \u{2192}", "move cursor"),
                ("Home  End", "start / end of input line"),
                ("End (empty)", "snap to bottom of history"),
                ("Backspace", "delete left"),
                ("Del", "delete right"),
                ("@", "mention a file"),
                (
                    "Ctrl+t",
                    "cycle thinking level (auto\u{2192}low\u{2192}medium\u{2192}high\u{2192}max)",
                ),
            ],
        },
        ShortcutSection {
            title: "sessions screen",
            rows: &[
                ("\u{2191} \u{2193}", "navigate sessions / groups"),
                ("Enter", "load session  /  collapse-expand group"),
                ("Del", "delete selected session"),
                ("type", "filter sessions by title or id"),
                ("Backspace", "clear last filter character"),
                ("q  Esc", "quit"),
            ],
        },
        ShortcutSection {
            title: "popups",
            rows: &[
                ("\u{2191} \u{2193}", "navigate"),
                ("Enter", "confirm"),
                ("Esc", "close"),
                ("type", "filter (sessions, models, themes)"),
            ],
        },
        ShortcutSection {
            title: "elicitation",
            rows: &[
                ("\u{2191} \u{2193}", "navigate fields / options"),
                ("Space", "toggle multi-select option"),
                ("Enter", "submit"),
                ("Esc", "decline"),
            ],
        },
    ]
}

pub(super) fn draw_help_popup(f: &mut Frame, app: &App) {
    let area = f.area();
    let popup_area = centered_rect(70, 80, area);

    f.render_widget(Clear, popup_area);
    f.render_widget(Block::default().style(Theme::popup_bg()), popup_area);

    let inner = Rect {
        x: popup_area.x + 1,
        y: popup_area.y + 1,
        width: popup_area.width.saturating_sub(2),
        height: popup_area.height.saturating_sub(2),
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // spacer
            Constraint::Min(1),    // list
            Constraint::Length(1), // hint
        ])
        .split(inner);

    // title
    f.render_widget(
        Paragraph::new(Span::styled("shortcuts", Theme::popup_title())).style(Theme::popup_bg()),
        chunks[0],
    );

    // shortcut list ───────────────────────────────────────────────────────────
    // Key column: 2-space left pad + key left-aligned in 12 chars = 14 total.
    const KEY_COL_W: usize = 14;

    let mut items: Vec<ListItem> = Vec::new();

    for (section_idx, section) in shortcut_sections().iter().enumerate() {
        // blank spacer row before every section except the first
        if section_idx > 0 {
            items.push(ListItem::new(Line::from(Span::raw(""))));
        }
        // section header
        items.push(ListItem::new(Line::from(Span::styled(
            format!("  {}", section.title),
            Theme::popup_title(),
        ))));
        // shortcut rows
        for &(key, desc) in section.rows {
            let key_col = format!("  {key:<KEY_COL_W$}");
            items.push(ListItem::new(Line::from(vec![
                Span::styled(key_col, Theme::status()),
                Span::styled(desc, Theme::popup_bg()),
            ])));
        }
    }

    let list = List::new(items).block(Block::default().style(Theme::popup_bg()));
    let mut state = ListState::default().with_offset(app.help_scroll);
    f.render_stateful_widget(list, chunks[2], &mut state);

    // hint
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {ARROW_UP}{ARROW_DOWN} scroll  esc close"),
            Theme::status(),
        ))
        .style(Theme::popup_bg()),
        chunks[3],
    );
}
