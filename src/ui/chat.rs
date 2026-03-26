use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Padding, Paragraph, Wrap},
};

use crate::app::{ActivityState, App, ChatEntry, SessionOp, ToolDetail};
use crate::markdown;
use crate::theme::Theme;

use super::{OUTCOME_BULLET, build_input_visual_layout, draw_header};

// ── Spinner ───────────────────────────────────────────────────────────────────

const BRAILLE_SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpinnerKind {
    Braille,
    Line,
    Dots,
}

const LINE_SPINNER: &[&str] = &["-", "\\", "|", "/"];
const DOTS_SPINNER: &[&str] = &[".  ", ".. ", "..."];

pub(super) fn spinner_frames(kind: SpinnerKind) -> &'static [&'static str] {
    match kind {
        SpinnerKind::Braille => BRAILLE_SPINNER,
        SpinnerKind::Line => LINE_SPINNER,
        SpinnerKind::Dots => DOTS_SPINNER,
    }
}

pub(crate) fn spinner(kind: SpinnerKind, tick: u64) -> &'static str {
    let frames = spinner_frames(kind);
    frames[(tick as usize / 2) % frames.len()]
}

// ── Elicitation symbols ───────────────────────────────────────────────────────
const RADIO_SELECTED: &str = "\u{25CF} "; // ● filled circle  – single-select active
const RADIO_UNSELECTED: &str = "\u{25CB} "; // ○ empty circle   – single-select inactive
const CHECK_CHECKED: &str = "\u{2611} "; // ☑ ballot box checked   – multi-select on
const CHECK_UNCHECKED: &str = "\u{2610} "; // ☐ ballot box unchecked – multi-select off

// ── Status bar icons ──────────────────────────────────────────────────────────
const ICON_CONTEXT: &str = "\u{1F5AA}"; // 🖪 document      – context token usage
const ICON_TOOLS: &str = "\u{2692}"; // ⚒  tools          – tool call count
pub(crate) const ICON_MULTI_SESSION: &str = "𐬽"; // multi-session recent activity indicator

// ── General text symbols ──────────────────────────────────────────────────────
const ARROW_UP: &str = "\u{2191}"; // ↑ upwards arrow

// ── Card types ────────────────────────────────────────────────────────────────

/// Cache for cards built from finalized messages.
/// Auto-invalidates when `messages` shrinks (clear/retain).
pub(crate) struct CardCache {
    pub(crate) cards: Vec<Card>,
    pub(crate) processed_messages: usize,
}

impl CardCache {
    pub(crate) fn new() -> Self {
        Self {
            cards: Vec::new(),
            processed_messages: 0,
        }
    }

    pub(crate) fn invalidate(&mut self) {
        self.cards.clear();
        self.processed_messages = 0;
    }
}

// -- message cards: background color only, no borders --

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CardKind {
    User,
    Assistant,
    Tool { compact: bool }, // compact=true: no top padding (follows assistant)
    Streaming,
    Thinking,
    Compaction,
    Error,
    Info,
    Elicitation,
}

pub(crate) struct Card {
    pub(crate) kind: CardKind,
    pub(crate) lines: Vec<Line<'static>>,
    top_pad: u16,
    bottom_pad: u16,
}

impl Card {
    pub(crate) fn new(kind: CardKind, lines: Vec<Line<'static>>) -> Self {
        let (top_pad, bottom_pad): (u16, u16) = match kind {
            CardKind::Tool { compact: true } => (0, 0),
            CardKind::Tool { compact: false } => (1, 0),
            _ => (1, 1),
        };
        Self {
            kind,
            lines,
            top_pad,
            bottom_pad,
        }
    }

    /// Compute the visual height of this card given the full card render width.
    /// Each logical line is wrapped at `(width - 4)` columns (the 2+2 horizontal
    /// padding used by `render()`), and the result is rounded up.
    pub(crate) fn height(&self, width: u16) -> u16 {
        let inner_w = width.saturating_sub(4) as usize;
        let line_rows: u16 = self
            .lines
            .iter()
            .map(|l| {
                let w = l.width();
                if inner_w == 0 || w == 0 {
                    1
                } else {
                    w.div_ceil(inner_w) as u16
                }
            })
            .sum::<u16>()
            .max(1);
        self.top_pad + line_rows + self.bottom_pad
    }

    /// `clip_top`: number of rows clipped off the top of this card
    fn render(&self, f: &mut Frame, area: Rect, clip_top: u16) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let (bg_style, text_style) = match self.kind {
            CardKind::User => (Theme::user_card(), Theme::user_text()),
            CardKind::Assistant | CardKind::Streaming => {
                (Theme::assistant_card(), Theme::assistant_text())
            }
            CardKind::Tool { .. } => (Theme::assistant_card(), Theme::tool_text()),
            CardKind::Thinking => (Theme::assistant_card(), Theme::thinking()),
            CardKind::Compaction => (Theme::assistant_card(), Theme::status_accent()),
            CardKind::Error => (Theme::assistant_card(), Theme::error_text()),
            CardKind::Info => (Theme::assistant_card(), Theme::info_text()),
            CardKind::Elicitation => (Theme::assistant_card(), Theme::status_accent()),
        };

        // fill background
        f.render_widget(Block::default().style(bg_style), area);

        // the card has: 1 row top padding, N content lines, 1 row bottom padding
        // when clipped, we skip the top padding first, then content lines
        let has_top_pad = !matches!(self.kind, CardKind::Tool { compact: true });
        let pad_top_visible = if clip_top == 0 && has_top_pad {
            1u16
        } else {
            0
        };
        let content_skip = clip_top.saturating_sub(1); // rows of content to skip

        let content_y = area.y + pad_top_visible;
        let content_h = area.height.saturating_sub(pad_top_visible);

        if content_h == 0 {
            return;
        }

        let content_area = Rect {
            x: area.x + 2,
            y: content_y,
            width: area.width.saturating_sub(4),
            height: content_h,
        };

        let styled_lines: Vec<Line<'static>> = self
            .lines
            .iter()
            .map(|l| {
                Line::from(
                    l.spans
                        .iter()
                        .map(|s| {
                            let style = if s.style.fg.is_some() {
                                s.style.bg(bg_style.bg.unwrap_or(Theme::bg()))
                            } else {
                                text_style
                            };
                            Span::styled(s.content.clone(), style)
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        f.render_widget(
            Paragraph::new(styled_lines)
                .wrap(Wrap { trim: false })
                .scroll((content_skip, 0)),
            content_area,
        );
    }
}

// ── Build message cards (cached) ──────────────────────────────────────────────

/// Build cards for finalized messages incrementally (cached).
/// Does NOT include the streaming/thinking card — that's built separately.
pub(crate) fn build_message_cards(app: &mut App) -> &[Card] {
    let cache = &app.card_cache;

    // Auto-invalidate if messages shrank (clear/retain)
    if app.messages.len() < cache.processed_messages {
        app.card_cache.invalidate();
    }

    // Cache hit — nothing new
    if app.messages.len() == app.card_cache.processed_messages {
        return &app.card_cache.cards;
    }

    // Determine where to start processing. If the last cached card is a tool
    // batch, we need to pop it and re-process from the batch start, because
    // new tool messages might need to merge into that batch.
    let start_idx = if matches!(
        app.card_cache.cards.last().map(|c| &c.kind),
        Some(CardKind::Tool { .. })
    ) {
        app.card_cache.cards.pop();
        // Scan backwards to find where the tool batch started
        let mut idx = app.card_cache.processed_messages;
        while idx > 0 && matches!(app.messages.get(idx - 1), Some(ChatEntry::ToolCall { .. })) {
            idx -= 1;
        }
        idx
    } else {
        app.card_cache.processed_messages
    };

    // Process new messages from start_idx
    let mut pending_tools: Vec<Line<'static>> = Vec::new();

    let flush_tools = |tools: &mut Vec<Line<'static>>, cards: &mut Vec<Card>| {
        if !tools.is_empty() {
            let lines = std::mem::take(tools);
            let prev_is_assistant = matches!(
                cards.last().map(|c| &c.kind),
                Some(CardKind::Assistant | CardKind::Streaming)
            );
            cards.push(Card::new(
                CardKind::Tool {
                    compact: prev_is_assistant,
                },
                lines,
            ));
        }
    };

    for entry in &app.messages[start_idx..] {
        match entry {
            ChatEntry::User { text, .. } => {
                flush_tools(&mut pending_tools, &mut app.card_cache.cards);
                let lines = markdown::render(text, Theme::user_text(), &app.hl);
                app.card_cache.cards.push(Card::new(CardKind::User, lines));
            }
            ChatEntry::Assistant { content, thinking } => {
                flush_tools(&mut pending_tools, &mut app.card_cache.cards);
                let mut lines = Vec::new();
                if app.show_thinking
                    && let Some(thinking_text) = thinking
                {
                    let mut rendered =
                        markdown::render(thinking_text, Theme::thinking_text(), &app.hl);
                    if let Some(first) = rendered.first_mut() {
                        first
                            .spans
                            .insert(0, Span::styled("\u{25CF} ", Theme::thinking()));
                    }
                    lines.extend(rendered);
                    lines.push(Line::default());
                }
                lines.extend(markdown::render(content, Theme::assistant_text(), &app.hl));
                app.card_cache
                    .cards
                    .push(Card::new(CardKind::Assistant, lines));
            }
            ChatEntry::ToolCall {
                name,
                is_error,
                detail,
                ..
            } => {
                let style = if *is_error {
                    Theme::tool_error()
                } else {
                    Theme::tool_text()
                };
                let sym = if *is_error { "x" } else { ">" };

                match detail {
                    ToolDetail::Edit {
                        file, cached_lines, ..
                    } => {
                        pending_tools.push(Line::from(vec![
                            Span::styled(format!("{sym} {name} "), style),
                            Span::styled(short_path(file).to_string(), Theme::diff_file()),
                        ]));
                        pending_tools.extend(cached_lines.iter().cloned());
                    }
                    ToolDetail::WriteFile {
                        path, cached_lines, ..
                    } => {
                        pending_tools.push(Line::from(vec![
                            Span::styled(format!("{sym} {name} "), style),
                            Span::styled(short_path(path).to_string(), Theme::diff_file()),
                        ]));
                        pending_tools.extend(cached_lines.iter().cloned());
                    }
                    ToolDetail::SummaryWithOutput { header, output } => {
                        pending_tools.push(Line::from(vec![
                            Span::styled(format!("{sym} {name} "), style),
                            Span::styled(header.clone(), Theme::diff_file()),
                        ]));
                        for line in output.lines() {
                            pending_tools.push(Line::from(Span::styled(
                                format!("  {line}"),
                                Theme::tool_output(),
                            )));
                        }
                    }
                    ToolDetail::Summary(info) => {
                        if info.contains('\n') {
                            pending_tools
                                .push(Line::from(Span::styled(format!("{sym} {name}"), style)));
                            for line in info.lines() {
                                pending_tools.push(Line::from(Span::styled(
                                    format!("  {line}"),
                                    Theme::diff_file(),
                                )));
                            }
                        } else {
                            pending_tools.push(Line::from(vec![
                                Span::styled(format!("{sym} {name} "), style),
                                Span::styled(info.clone(), Theme::diff_file()),
                            ]));
                        }
                    }
                    ToolDetail::None => {
                        pending_tools
                            .push(Line::from(Span::styled(format!("{sym} {name}"), style)));
                    }
                }
            }
            ChatEntry::CompactionStart { token_estimate } => {
                flush_tools(&mut pending_tools, &mut app.card_cache.cards);
                let token_str = format!("~{} tokens", token_estimate);
                app.card_cache.cards.push(Card::new(
                    CardKind::Compaction,
                    vec![
                        Line::from(vec![
                            Span::styled("[compact] ", Theme::status_accent()),
                            Span::styled(
                                "Summarizing conversation history",
                                Theme::status_accent(),
                            ),
                        ]),
                        Line::from(Span::styled(format!("  {token_str}"), Theme::status())),
                    ],
                ));
            }
            ChatEntry::CompactionEnd {
                token_estimate,
                summary,
                summary_len,
            } => {
                flush_tools(&mut pending_tools, &mut app.card_cache.cards);
                let mut lines = vec![Line::from(vec![
                    Span::styled("[compact] ", Theme::status_accent()),
                    Span::styled("Conversation summarized", Theme::status_accent()),
                ])];
                if let Some(token_estimate) = token_estimate {
                    lines.push(Line::from(Span::styled(
                        format!("  ~{} tokens -> {} chars", token_estimate, summary_len),
                        Theme::status(),
                    )));
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("  {} chars", summary_len),
                        Theme::status(),
                    )));
                }
                lines.push(Line::default());
                lines.extend(markdown::render(summary, Theme::assistant_text(), &app.hl));
                app.card_cache
                    .cards
                    .push(Card::new(CardKind::Compaction, lines));
            }
            ChatEntry::Info(text) => {
                flush_tools(&mut pending_tools, &mut app.card_cache.cards);
                app.card_cache
                    .cards
                    .push(Card::new(CardKind::Info, vec![Line::from(text.clone())]));
            }
            ChatEntry::Error(text) => {
                flush_tools(&mut pending_tools, &mut app.card_cache.cards);
                app.card_cache
                    .cards
                    .push(Card::new(CardKind::Error, vec![Line::from(text.clone())]));
            }
            ChatEntry::Elicitation {
                message,
                source: _,
                outcome,
                ..
            } => {
                flush_tools(&mut pending_tools, &mut app.card_cache.cards);
                let header = Line::from(vec![
                    Span::styled("[?] ", Theme::status_accent()),
                    Span::styled(message.clone(), Theme::status_accent()),
                ]);
                let mut card_lines = vec![header];
                match outcome.as_deref() {
                    None => {
                        card_lines.push(Line::from(Span::styled(
                            "  waiting for response\u{2026}",
                            Theme::thinking(),
                        )));
                    }
                    Some("declined") | Some("cancelled") => {
                        card_lines.push(Line::from(Span::styled(
                            format!("  {}", outcome.as_deref().unwrap()),
                            Theme::status(),
                        )));
                    }
                    Some(text) => {
                        for part in text.lines() {
                            card_lines.push(Line::from(Span::styled(
                                format!("  {part}"),
                                Theme::info_text(),
                            )));
                        }
                    }
                }
                app.card_cache
                    .cards
                    .push(Card::new(CardKind::Elicitation, card_lines));
            }
        }
    }

    flush_tools(&mut pending_tools, &mut app.card_cache.cards);
    app.card_cache.processed_messages = app.messages.len();

    &app.card_cache.cards
}

// ── Draw chat screen ──────────────────────────────────────────────────────────

pub(super) fn draw_chat(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let mention_height = if app.mention_state.is_some()
        || app.file_index_loading
        || app.file_index_error.is_some()
    {
        6
    } else {
        0
    };

    // Compute how many visual rows the input text needs when wrapped.
    let input_inner_width = area.width.saturating_sub(4) as usize;
    let prefix_width = 2usize; // "> "
    let input_layout = build_input_visual_layout(
        &app.input,
        app.input_cursor,
        input_inner_width.max(1),
        prefix_width,
    );
    let max_input_lines: u16 = 5;
    let input_height = (input_layout.total_rows() as u16).clamp(1, max_input_lines) + 1; // +1 bottom padding

    // Elicitation popup height: header (2) + message (1) + blank (1) + options/field + hint (1)
    let elicitation_height: u16 = if let Some(state) = &app.elicitation {
        let option_rows = state.current_option_count() as u16;
        // header line + message line + blank + options-or-input (min 1) + hint line
        (2 + 1 + option_rows.max(1) + 1).min(area.height / 2)
    } else {
        0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(3),    // messages
            Constraint::Length(mention_height),
            Constraint::Length(elicitation_height), // elicitation popup (0 when inactive)
            Constraint::Length(1),                  // input border line
            Constraint::Length(input_height),       // input (dynamic)
        ])
        .split(area);

    // header
    let model_str = match (&app.current_provider, &app.current_model) {
        (Some(p), Some(m)) => format!("{p}/{m}"),
        _ => "no model".into(),
    };
    let sid = app
        .session_id
        .as_deref()
        .map(|s| if s.len() > 8 { &s[..8] } else { s })
        .unwrap_or("???");

    let mut right_spans: Vec<Span<'static>> = Vec::new();

    // time
    if let Some(dur) = app.llm_request_elapsed() {
        let secs = dur.as_secs();
        let time_str = if secs < 60 {
            format!("{secs}s")
        } else {
            format!("{}m{}s", secs / 60, secs % 60)
        };
        right_spans.push(Span::styled(format!(" {time_str} "), Theme::status()));
    }

    // context %
    if let Some(context_tokens) = app.session_stats.latest_context_tokens
        && context_tokens > 0
        && app.context_limit > 0
    {
        let pct = (context_tokens as f64 / app.context_limit as f64 * 100.0) as u32;
        right_spans.push(Span::styled(
            format!(" {ICON_CONTEXT} {pct}% "),
            Theme::status(),
        ));
    }

    // tool calls
    if app.session_stats.total_tool_calls > 0 {
        right_spans.push(Span::styled(
            format!(" {ICON_TOOLS} {} ", app.session_stats.total_tool_calls),
            Theme::status(),
        ));
    }

    // cost
    if let Some(cost) = app.cumulative_cost
        && cost > 0.0
    {
        right_spans.push(Span::styled(
            format!(" ${cost:.4} "),
            Theme::status_accent(),
        ));
    }

    // show a badge when other sessions have produced recent websocket activity.
    let other_active_session_count = app.other_active_session_count();
    if other_active_session_count > 0 {
        right_spans.push(Span::styled(
            format!(" {ICON_MULTI_SESSION} {other_active_session_count} "),
            Theme::status(),
        ));
    }

    // model + thinking level: " provider/model:level "
    let effort_label = app.reasoning_effort_label().to_string();
    right_spans.push(Span::styled(
        format!(" {model_str}"),
        Theme::status_accent(),
    ));
    right_spans.push(Span::styled(":", Theme::reasoning_effort_sep()));
    right_spans.push(Span::styled(
        format!("{effort_label} "),
        Theme::reasoning_effort_level(),
    ));

    draw_header(
        f,
        app,
        chunks[0],
        vec![
            Span::styled(format!(" {sid}"), Theme::status()),
            Span::styled(
                format!(" {} ", app.agent_mode),
                Theme::mode_badge(&app.agent_mode),
            ),
            Span::styled(format!(" {}", app.status), Theme::status()),
        ],
        right_spans,
    );

    // messages
    draw_messages(f, app, chunks[1]);

    if mention_height > 0 {
        draw_mention_panel(f, app, chunks[2]);
    }

    if elicitation_height > 0 {
        draw_elicitation_popup(f, app, chunks[3]);
    }

    // input border line reflects active session state
    let border_style = match &app.activity {
        ActivityState::SessionOp(SessionOp::Undo) => Theme::input_border_undo(),
        ActivityState::SessionOp(SessionOp::Redo) => Theme::input_border_redo(),
        _ if app.cancel_confirm_active() => Theme::input_border_cancel_confirm(),
        ActivityState::Compacting { .. } => Theme::input_border_compacting(),
        ActivityState::Thinking | ActivityState::Streaming | ActivityState::RunningTool { .. } => {
            Theme::input_border_thinking()
        }
        _ if app.elicitation.is_some() => Theme::input_border_thinking(), // accent while waiting
        _ => Theme::mode_border(&app.agent_mode),
    };
    let border_line = Paragraph::new("▔".repeat(chunks[4].width as usize)).style(border_style);
    f.render_widget(border_line, chunks[4]);

    // input area
    let input_bg = Block::default()
        .padding(Padding::new(2, 2, 0, 1))
        .style(Theme::input());
    let inner = input_bg.inner(chunks[5]);
    app.input_line_width = (inner.width as usize).max(1);
    f.render_widget(input_bg, chunks[5]);

    let (label_text, label_style) = match &app.activity {
        ActivityState::SessionOp(SessionOp::Undo) => (
            format!("{} undoing ", spinner(SpinnerKind::Braille, app.tick)),
            Theme::input_undo(),
        ),
        ActivityState::SessionOp(SessionOp::Redo) => (
            format!("{} redoing ", spinner(SpinnerKind::Braille, app.tick)),
            Theme::input_redo(),
        ),
        _ if app.cancel_confirm_active() => (
            format!(
                "{} Esc again to stop ",
                spinner(SpinnerKind::Braille, app.tick)
            ),
            Theme::input_cancel_confirm(),
        ),
        ActivityState::Compacting { .. }
        | ActivityState::RunningTool { .. }
        | ActivityState::Thinking
        | ActivityState::Streaming => (
            format!("{} ", spinner(SpinnerKind::Braille, app.tick)),
            Theme::input_thinking(),
        ),
        _ if app.elicitation.is_some() => (
            format!("  answer above {ARROW_UP} "),
            Theme::input_thinking(),
        ),
        _ => ("> ".into(), Theme::mode_border(&app.agent_mode)),
    };
    let input_style = Theme::input();
    let hide_input_contents = app.should_hide_input_contents();

    if hide_input_contents {
        app.input_scroll = 0;
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(label_text, label_style))),
            inner,
        );
    } else {
        let layout = build_input_visual_layout(
            &app.input,
            app.input_cursor,
            app.input_line_width,
            prefix_width,
        );
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (idx, row) in layout.rows.iter().enumerate() {
            if idx == 0 {
                lines.push(Line::from(vec![
                    Span::styled(label_text.clone(), label_style),
                    Span::styled(row.text.clone(), input_style),
                ]));
            } else {
                lines.push(Line::from(Span::styled(row.text.clone(), input_style)));
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled("", input_style)));
        }

        let total_lines = lines.len() as u16;
        let visible = inner.height;
        if (layout.cursor_row as u16) >= app.input_scroll + visible {
            app.input_scroll = (layout.cursor_row as u16) - visible + 1;
        }
        if (layout.cursor_row as u16) < app.input_scroll {
            app.input_scroll = layout.cursor_row as u16;
        }
        app.input_scroll = app.input_scroll.min(total_lines.saturating_sub(visible));

        f.render_widget(Paragraph::new(lines).scroll((app.input_scroll, 0)), inner);

        let visual_row = (layout.cursor_row as u16).saturating_sub(app.input_scroll);
        if visual_row < visible {
            f.set_cursor_position((inner.x + layout.cursor_col as u16, inner.y + visual_row));
        }
    }
}

// ── Elicitation popup ─────────────────────────────────────────────────────────

fn draw_elicitation_popup(f: &mut Frame, app: &mut App, area: Rect) {
    use crate::app::ElicitationFieldKind;

    let Some(state) = &app.elicitation else {
        return;
    };
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Background
    f.render_widget(Block::default().style(Theme::popup_bg()), area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut row = inner.y;
    let max_y = inner.y + inner.height;

    // ── Title row ────────────────────────────────────────────────────────────
    if row < max_y {
        let source_label = state.source_label().to_string();
        let title = Line::from(vec![
            Span::styled(OUTCOME_BULLET, Theme::status_accent()),
            Span::styled("Question", Theme::status_accent()),
            Span::styled(format!("  via {source_label}"), Theme::status()),
        ]);
        f.render_widget(
            Paragraph::new(title).style(Theme::popup_bg()),
            Rect {
                x: inner.x,
                y: row,
                width: inner.width,
                height: 1,
            },
        );
        row += 1;
    }

    // ── Message ───────────────────────────────────────────────────────────────
    if row < max_y {
        f.render_widget(
            Paragraph::new(Span::styled(format!("  {}", state.message), Theme::fg()))
                .style(Theme::popup_bg()),
            Rect {
                x: inner.x,
                y: row,
                width: inner.width,
                height: 1,
            },
        );
        row += 1;
    }

    // ── Field content ─────────────────────────────────────────────────────────
    if row < max_y {
        let field = state.current_field();
        match &field.kind {
            ElicitationFieldKind::SingleSelect { options }
            | ElicitationFieldKind::MultiSelect { options } => {
                let is_multi = matches!(&field.kind, ElicitationFieldKind::MultiSelect { .. });
                let selected_vals = state.selected.get(&field.name);
                for (idx, opt) in options.iter().enumerate() {
                    if row >= max_y {
                        break;
                    }
                    let highlighted = idx == state.option_cursor;
                    let is_chosen = if is_multi {
                        if let Some(serde_json::Value::Array(arr)) = selected_vals {
                            arr.contains(&opt.value)
                        } else {
                            false
                        }
                    } else {
                        selected_vals == Some(&opt.value)
                    };

                    let bullet = if is_multi {
                        if is_chosen {
                            CHECK_CHECKED
                        } else {
                            CHECK_UNCHECKED
                        }
                    } else if highlighted {
                        RADIO_SELECTED
                    } else {
                        RADIO_UNSELECTED
                    };
                    let style = if highlighted {
                        Theme::status_accent()
                    } else {
                        Theme::status()
                    };
                    let line = Line::from(vec![
                        Span::styled(format!("  {bullet}"), style),
                        Span::styled(opt.label.clone(), style),
                        if let Some(desc) = &opt.description {
                            Span::styled(format!("  {desc}"), Theme::dim())
                        } else {
                            Span::raw("")
                        },
                    ]);
                    f.render_widget(
                        Paragraph::new(line).style(Theme::popup_bg()),
                        Rect {
                            x: inner.x,
                            y: row,
                            width: inner.width,
                            height: 1,
                        },
                    );
                    row += 1;
                }
            }
            ElicitationFieldKind::TextInput | ElicitationFieldKind::NumberInput { .. } => {
                let placeholder = if matches!(&field.kind, ElicitationFieldKind::NumberInput { .. })
                {
                    "enter number\u{2026}"
                } else {
                    "enter text\u{2026}"
                };
                let display = if state.text_input.is_empty() {
                    Span::styled(placeholder, Theme::dim())
                } else {
                    Span::styled(state.text_input.clone(), Theme::fg())
                };
                let line = Line::from(vec![Span::styled("  > ", Theme::status_accent()), display]);
                f.render_widget(
                    Paragraph::new(line).style(Theme::popup_bg()),
                    Rect {
                        x: inner.x,
                        y: row,
                        width: inner.width,
                        height: 1,
                    },
                );
                row += 1;
            }
            ElicitationFieldKind::BooleanToggle => {
                let val = state
                    .selected
                    .get(&field.name)
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let line = Line::from(vec![
                    Span::styled(
                        if val {
                            format!("  {CHECK_CHECKED}Yes")
                        } else {
                            format!("  {CHECK_UNCHECKED}No")
                        },
                        Theme::status_accent(),
                    ),
                    Span::styled("  (Space to toggle)", Theme::dim()),
                ]);
                f.render_widget(
                    Paragraph::new(line).style(Theme::popup_bg()),
                    Rect {
                        x: inner.x,
                        y: row,
                        width: inner.width,
                        height: 1,
                    },
                );
                row += 1;
            }
        }
    }

    // ── Hint row ──────────────────────────────────────────────────────────────
    if row < max_y {
        let arrow_up = super::ARROW_UP;
        let arrow_down = super::ARROW_DOWN;
        let hint = match state.current_field().kind {
            ElicitationFieldKind::MultiSelect { .. } => {
                format!(" {arrow_up}{arrow_down} navigate  Space toggle  Enter submit  Esc decline")
            }
            ElicitationFieldKind::TextInput | ElicitationFieldKind::NumberInput { .. } => {
                " type answer  Enter submit  Esc decline".to_string()
            }
            ElicitationFieldKind::BooleanToggle => {
                " Space toggle  Enter submit  Esc decline".to_string()
            }
            ElicitationFieldKind::SingleSelect { .. } => {
                format!(" {arrow_up}{arrow_down} navigate  Enter select  Esc decline")
            }
        };
        f.render_widget(
            Paragraph::new(Span::styled(hint, Theme::dim())).style(Theme::popup_bg()),
            Rect {
                x: inner.x,
                y: row,
                width: inner.width,
                height: 1,
            },
        );
    }
}

// ── Mention panel ─────────────────────────────────────────────────────────────

fn draw_mention_panel(f: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let mut items: Vec<ListItem> = Vec::new();
    if app.file_index_loading && app.file_index.is_empty() {
        items.push(ListItem::new(Line::from(vec![Span::styled(
            format!("{} indexing files", spinner(SpinnerKind::Braille, app.tick)),
            Theme::thinking(),
        )])));
    } else if let Some(error) = &app.file_index_error {
        items.push(ListItem::new(Line::from(vec![Span::styled(
            format!("file index error: {error}"),
            Theme::error_text(),
        )])));
    } else if let Some(mention) = &app.mention_state {
        if mention.results.is_empty() {
            items.push(ListItem::new(Line::from(vec![Span::styled(
                format!("no matches for @{}", mention.query),
                Theme::info_text(),
            )])));
        } else {
            for entry in &mention.results {
                let icon = if entry.is_dir { "[D]" } else { "[F]" };
                items.push(ListItem::new(Line::from(vec![
                    Span::styled(format!("{icon} "), Theme::status()),
                    Span::styled(entry.path.clone(), Theme::input()),
                ])));
            }
        }
    }

    if items.is_empty() {
        return;
    }

    let title = if let Some(mention) = &app.mention_state {
        format!(" @ files - {} ", mention.query)
    } else {
        " @ files ".into()
    };
    let list = List::new(items)
        .block(Block::default().title(title).style(Theme::popup_bg()))
        .highlight_style(Theme::selected())
        .highlight_symbol("");
    let selected = app
        .mention_state
        .as_ref()
        .map(|mention| mention.selected_index)
        .filter(|_| !app.file_index_loading);
    let mut state = ListState::default().with_selected(selected);
    f.render_stateful_widget(list, area, &mut state);
}

// ── Streaming card (transient, not cached) ────────────────────────────────────

/// Build the transient streaming/thinking card (not cached, rebuilt every frame).
fn build_streaming_card(app: &mut App) -> Option<Card> {
    let activity_text = match &app.activity {
        ActivityState::RunningTool { name } => {
            format!("{} tool: {name}", spinner(SpinnerKind::Braille, app.tick))
        }
        ActivityState::Compacting { .. } => {
            format!("{} compacting", spinner(SpinnerKind::Braille, app.tick))
        }
        ActivityState::Streaming => {
            format!("{} streaming", spinner(SpinnerKind::Braille, app.tick))
        }
        _ => format!("{} thinking", spinner(SpinnerKind::Braille, app.tick)),
    };

    let has_thinking = app.show_thinking && !app.streaming_thinking.is_empty();
    let has_content = !app.streaming_content.is_empty();

    if has_thinking || has_content {
        let mut lines = Vec::new();

        if has_thinking {
            let thinking_len = app.streaming_thinking.len();
            let mut thinking_lines =
                if let Some(cached) = app.streaming_thinking_cache.get(thinking_len) {
                    cached.to_vec()
                } else {
                    let rendered =
                        markdown::render(&app.streaming_thinking, Theme::thinking_text(), &app.hl);
                    app.streaming_thinking_cache
                        .store(thinking_len, rendered.clone());
                    rendered
                };
            if let Some(first) = thinking_lines.first_mut() {
                first
                    .spans
                    .insert(0, Span::styled("\u{25CF} ", Theme::thinking()));
            }
            lines.extend(thinking_lines);
            if has_content {
                lines.push(Line::default());
            }
        }

        if has_content {
            let content_len = app.streaming_content.len();
            let content_lines = if let Some(cached) = app.streaming_cache.get(content_len) {
                cached.to_vec()
            } else {
                let rendered =
                    markdown::render(&app.streaming_content, Theme::assistant_text(), &app.hl);
                app.streaming_cache.store(content_len, rendered.clone());
                rendered
            };
            lines.extend(content_lines);
        }

        lines.push(Line::from(Span::styled(activity_text, Theme::thinking())));
        Some(Card::new(CardKind::Streaming, lines))
    } else if app.is_turn_active() {
        Some(Card::new(
            CardKind::Thinking,
            vec![Line::from(Span::styled(activity_text, Theme::thinking()))],
        ))
    } else {
        None
    }
}

// ── Diff / write helpers ──────────────────────────────────────────────────────

fn short_path(path: &str) -> &str {
    // show last 2 components
    let mut count = 0;
    for (i, c) in path.char_indices().rev() {
        if c == '/' {
            count += 1;
            if count == 2 {
                return &path[i + 1..];
            }
        }
    }
    path
}

pub(crate) fn build_diff_lines(
    old: &str,
    new: &str,
    start_line: Option<usize>,
) -> Vec<Line<'static>> {
    use similar::{ChangeTag, TextDiff};

    let mut lines = Vec::new();
    let start = start_line.unwrap_or(1);

    let diff = TextDiff::from_lines(old, new);

    // compute max line number for gutter width
    let old_count = old.lines().count();
    let new_count = new.lines().count();
    let max_line = start + old_count.max(new_count);
    let gw = max_line.to_string().len();

    let mut old_line_idx = 0usize;
    let mut new_line_idx = 0usize;

    // group changes to pair up Delete/Insert for inline highlighting
    let changes: Vec<_> = diff.iter_all_changes().collect();
    let mut i = 0;
    while i < changes.len() {
        let change = &changes[i];
        match change.tag() {
            ChangeTag::Equal => {
                let ln = format!("{:>w$}", start + old_line_idx, w = gw);
                lines.push(Line::from(vec![
                    Span::styled(format!("  {ln} "), Theme::diff_context()),
                    Span::styled(
                        format!("  {}", change.value().trim_end_matches('\n')),
                        Theme::diff_context(),
                    ),
                ]));
                old_line_idx += 1;
                new_line_idx += 1;
                i += 1;
            }
            ChangeTag::Delete => {
                // collect consecutive deletes
                let del_start = i;
                while i < changes.len() && changes[i].tag() == ChangeTag::Delete {
                    i += 1;
                }
                let del_end = i;
                // collect consecutive inserts right after
                let ins_start = i;
                while i < changes.len() && changes[i].tag() == ChangeTag::Insert {
                    i += 1;
                }
                let ins_end = i;

                let dels: Vec<&str> = changes[del_start..del_end]
                    .iter()
                    .map(|c| c.value().trim_end_matches('\n'))
                    .collect();
                let inss: Vec<&str> = changes[ins_start..ins_end]
                    .iter()
                    .map(|c| c.value().trim_end_matches('\n'))
                    .collect();

                // render paired lines with inline char diff
                let paired = dels.len().min(inss.len());
                for j in 0..paired {
                    let (del_spans, ins_spans) = inline_diff(dels[j], inss[j]);

                    let ln_old = format!("{:>w$}", start + old_line_idx, w = gw);
                    let mut del_line = vec![
                        Span::styled(format!("  {ln_old} "), Theme::diff_context()),
                        Span::styled("- ", Theme::diff_removed()),
                    ];
                    del_line.extend(del_spans);
                    lines.push(Line::from(del_line));

                    let ln_new = format!("{:>w$}", start + new_line_idx, w = gw);
                    let mut ins_line = vec![
                        Span::styled(format!("  {ln_new} "), Theme::diff_context()),
                        Span::styled("+ ", Theme::diff_added()),
                    ];
                    ins_line.extend(ins_spans);
                    lines.push(Line::from(ins_line));

                    old_line_idx += 1;
                    new_line_idx += 1;
                }

                // remaining unpaired deletes
                for del in dels.iter().skip(paired) {
                    let ln = format!("{:>w$}", start + old_line_idx, w = gw);
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {ln} "), Theme::diff_context()),
                        Span::styled(format!("- {del}"), Theme::diff_removed()),
                    ]));
                    old_line_idx += 1;
                }
                // remaining unpaired inserts
                for ins in inss.iter().skip(paired) {
                    let ln = format!("{:>w$}", start + new_line_idx, w = gw);
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {ln} "), Theme::diff_context()),
                        Span::styled(format!("+ {ins}"), Theme::diff_added()),
                    ]));
                    new_line_idx += 1;
                }
            }
            ChangeTag::Insert => {
                // standalone insert (not preceded by delete)
                let ln = format!("{:>w$}", start + new_line_idx, w = gw);
                lines.push(Line::from(vec![
                    Span::styled(format!("  {ln} "), Theme::diff_context()),
                    Span::styled(
                        format!("+ {}", change.value().trim_end_matches('\n')),
                        Theme::diff_added(),
                    ),
                ]));
                new_line_idx += 1;
                i += 1;
            }
        }
    }

    lines
}

/// Char-level diff between two lines. Returns (old_spans, new_spans) with
/// changed ranges highlighted.
fn inline_diff(old: &str, new: &str) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_words(old, new);

    // build two flat lists: (text, is_highlighted) for old and new
    let mut old_parts: Vec<(String, bool)> = Vec::new();
    let mut new_parts: Vec<(String, bool)> = Vec::new();

    fn push_part(parts: &mut Vec<(String, bool)>, text: &str, highlighted: bool) {
        if let Some(last) = parts.last_mut()
            && last.1 == highlighted
        {
            last.0.push_str(text);
            return;
        }
        parts.push((text.to_string(), highlighted));
    }

    for change in diff.iter_all_changes() {
        let val = change.value();
        match change.tag() {
            ChangeTag::Equal => {
                push_part(&mut old_parts, val, false);
                push_part(&mut new_parts, val, false);
            }
            ChangeTag::Delete => {
                push_part(&mut old_parts, val, true);
            }
            ChangeTag::Insert => {
                push_part(&mut new_parts, val, true);
            }
        }
    }

    let old_spans = old_parts
        .into_iter()
        .map(|(text, hl)| {
            if hl {
                Span::styled(text, Theme::diff_removed_hl())
            } else {
                Span::styled(text, Theme::diff_removed())
            }
        })
        .collect();

    let new_spans = new_parts
        .into_iter()
        .map(|(text, hl)| {
            if hl {
                Span::styled(text, Theme::diff_added_hl())
            } else {
                Span::styled(text, Theme::diff_added())
            }
        })
        .collect();

    (old_spans, new_spans)
}

pub(crate) fn build_write_lines(content: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let total = content.lines().count();
    let gw = total.to_string().len();
    let max_preview = 20;
    for (i, l) in content.lines().enumerate() {
        if i >= max_preview {
            lines.push(Line::from(Span::styled(
                format!(
                    "  {:>w$}   ... ({} more lines)",
                    "",
                    total - max_preview,
                    w = gw
                ),
                Theme::diff_context(),
            )));
            break;
        }
        let ln = format!("{:>w$}", i + 1, w = gw);
        lines.push(Line::from(vec![
            Span::styled(format!("  {ln} "), Theme::diff_context()),
            Span::styled(format!("+ {l}"), Theme::diff_added()),
        ]));
    }
    lines
}

// ── Draw messages area ────────────────────────────────────────────────────────

fn draw_messages(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_widget(Block::default().style(Theme::base()), area);

    // Update cached message cards incrementally, then build transient card
    build_message_cards(app);
    let streaming_card = build_streaming_card(app);

    // Compute total_height in a temporary scope so we can mutably access app
    // for scroll compensation before borrowing card_cache again for rendering.
    let total_height: u16 = {
        let cards = app.card_cache.cards.iter().chain(streaming_card.iter());
        cards.map(|c| c.height(area.width)).sum()
    };

    if total_height == 0 && app.card_cache.cards.is_empty() && streaming_card.is_none() {
        return;
    }

    // When the user is scrolled up, bump scroll_offset by however much
    // content grew so the viewport stays at the same absolute position.
    app.compensate_scroll_for_growth(total_height);

    // max scroll = how far we can scroll from the bottom
    let max_scroll = total_height.saturating_sub(area.height);
    // clamp so we never scroll past the top
    app.scroll_offset = app.scroll_offset.min(max_scroll);
    // scroll_offset 0 = pinned to bottom, higher = scrolled up
    let scroll = max_scroll.saturating_sub(app.scroll_offset);

    let all_cards: Vec<&Card> = app
        .card_cache
        .cards
        .iter()
        .chain(streaming_card.iter())
        .collect();

    let mut y: i32 = -(scroll as i32);
    for card in &all_cards {
        let card_h = card.height(area.width);
        let card_top = y;
        let card_bottom = y + card_h as i32;

        if card_bottom > 0 && card_top < area.height as i32 {
            let render_y = card_top.max(0) as u16;
            let visible_h = (card_bottom.min(area.height as i32) - render_y as i32) as u16;
            let clip_top = (-card_top).max(0) as u16; // rows clipped off top

            card.render(
                f,
                Rect {
                    x: area.x,
                    y: area.y + render_y,
                    width: area.width,
                    height: visible_h.min(card_h),
                },
                clip_top,
            );
        }

        y += card_h as i32;
    }
}
