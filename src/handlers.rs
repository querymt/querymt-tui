use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use crate::app::{self, ActivityState, App, Popup, Screen};
use crate::config;
use crate::protocol::{self, ClientMsg, PromptBlock};
use crate::theme;
use crate::ui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppAction {
    None,
    OpenExternalEditor,
}

pub(crate) fn can_send_server_commands(app: &mut App) -> bool {
    if app.conn == app::ConnState::Connected {
        true
    } else {
        app.set_status(
            app::LogLevel::Warn,
            "connection",
            "not connected - waiting to reconnect",
        );
        false
    }
}

/// Handle all keyboard input while an elicitation popup is active.
///
/// Returns `Ok(())` in all cases; the caller should return immediately after
/// this to avoid routing the key to the normal chat handler.
pub(crate) fn handle_elicitation_key(
    app: &mut App,
    key: KeyEvent,
    cmd_tx: &mpsc::UnboundedSender<ClientMsg>,
) -> anyhow::Result<()> {
    use app::ElicitationFieldKind;

    // Take a snapshot of what we need before the mutable borrow on app.
    let Some(state) = app.elicitation.as_mut() else {
        return Ok(());
    };

    match key.code {
        // ── Cancel / decline ─────────────────────────────────────────────────
        KeyCode::Esc => {
            let eid = state.elicitation_id.clone();
            cmd_tx.send(ClientMsg::ElicitationResponse {
                elicitation_id: eid.clone(),
                action: "decline".into(),
                content: None,
            })?;
            app.resolve_elicitation(&eid, "declined");
        }

        // ── Navigation (select fields) ────────────────────────────────────────
        KeyCode::Down => {
            state.move_cursor(1);
        }
        KeyCode::Up => {
            state.move_cursor(-1);
        }

        // ── Space: toggle multi-select ────────────────────────────────────────
        KeyCode::Char(' ') => {
            let kind = state.current_field().kind.clone();
            if matches!(kind, ElicitationFieldKind::MultiSelect { .. }) {
                state.toggle_current_option();
            }
        }

        // ── Submit ────────────────────────────────────────────────────────────
        KeyCode::Enter => {
            let kind = state.current_field().kind.clone();
            match kind {
                ElicitationFieldKind::SingleSelect { .. } => {
                    // Select the highlighted option first
                    state.select_current_option();
                }
                ElicitationFieldKind::TextInput
                | ElicitationFieldKind::NumberInput { .. }
                | ElicitationFieldKind::BooleanToggle
                | ElicitationFieldKind::MultiSelect { .. } => {
                    // Already captured in state; fall through to submit
                }
            }

            if state.is_valid() {
                let eid = state.elicitation_id.clone();
                let content = state.build_accept_content();
                let display = state.selected_display();
                cmd_tx.send(ClientMsg::ElicitationResponse {
                    elicitation_id: eid.clone(),
                    action: "accept".into(),
                    content: Some(content),
                })?;
                app.resolve_elicitation(&eid, &display);
            }
            // If not valid (required field missing), do nothing — keep popup open.
        }

        // ── Text / number input ───────────────────────────────────────────────
        KeyCode::Char(c) => {
            let kind = state.current_field().kind.clone();
            if matches!(
                kind,
                ElicitationFieldKind::TextInput | ElicitationFieldKind::NumberInput { .. }
            ) {
                let cursor = state.text_cursor;
                state.text_input.insert(cursor, c);
                state.text_cursor += c.len_utf8();
            }
        }
        KeyCode::Backspace => {
            let kind = state.current_field().kind.clone();
            if matches!(
                kind,
                ElicitationFieldKind::TextInput | ElicitationFieldKind::NumberInput { .. }
            ) && state.text_cursor > 0
            {
                // Remove the char just before the cursor
                let cursor = state.text_cursor;
                let ch = state.text_input[..cursor].chars().next_back().unwrap();
                let ch_len = ch.len_utf8();
                state.text_input.remove(cursor - ch_len);
                state.text_cursor -= ch_len;
            }
        }

        _ => {}
    }
    Ok(())
}

/// If the app has a stored model preference for its current agent mode, and the
/// active session is running a different model, send a `SetSessionModel` message
/// to switch automatically.  Mirrors the `useEffect` in `AppShell.tsx`.
pub(crate) fn apply_mode_model_if_preferred(
    app: &mut App,
    cmd_tx: &mpsc::UnboundedSender<ClientMsg>,
) -> anyhow::Result<()> {
    let Some(sid) = app.session_id.clone() else {
        return Ok(());
    };
    let Some((provider, model)) = app.get_mode_model_preference(&app.agent_mode) else {
        return Ok(());
    };
    let differs = app.current_provider.as_deref() != Some(provider)
        || app.current_model.as_deref() != Some(model);
    if !differs {
        return Ok(());
    }
    // Resolve the canonical model entry so we have the right model_id / node_id.
    let provider = provider.to_string();
    let model = model.to_string();
    if let Some(entry) = app
        .models
        .iter()
        .find(|m| m.provider == provider && m.model == model && m.node_id.is_none())
        .cloned()
    {
        app.current_provider = Some(entry.provider.clone());
        app.current_model = Some(entry.model.clone());
        app.set_status(
            app::LogLevel::Info,
            "model",
            format!("mode: {} → model: {}", app.agent_mode, entry.label),
        );
        cmd_tx.send(ClientMsg::SetSessionModel {
            session_id: sid,
            model_id: entry.id,
            node_id: entry.node_id,
        })?;
    }
    Ok(())
}

pub(crate) fn handle_key(
    app: &mut App,
    key: KeyEvent,
    cmd_tx: &mpsc::UnboundedSender<ClientMsg>,
) -> anyhow::Result<AppAction> {
    if key.code != KeyCode::Esc && app.pending_cancel_confirm_until.is_some() {
        app.clear_cancel_confirm();
        app.refresh_transient_status();
    }

    // ctrl-c: clear input first, quit on second press
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if !app.input.is_empty() {
            app.input.clear();
            app.input_cursor = 0;
            app.input_scroll = 0;
        } else {
            app.should_quit = true;
        }
        return Ok(AppAction::None);
    }

    // chord second key: ctrl+x was pressed, now handle the follow-up
    if app.chord {
        app.chord = false;
        app.set_status(app::LogLevel::Debug, "input", "ready");
        if key.code == KeyCode::Char('e') {
            if app.screen != Screen::Chat {
                app.set_status(
                    app::LogLevel::Warn,
                    "editor",
                    "external editor is only available in chat",
                );
                return Ok(AppAction::None);
            }
            return Ok(AppAction::OpenExternalEditor);
        }
        handle_chord(app, key, cmd_tx)?;
        return Ok(AppAction::None);
    }

    // elicitation popup takes full control of input when active
    if app.elicitation.is_some() {
        handle_elicitation_key(app, key, cmd_tx)?;
        return Ok(AppAction::None);
    }

    // direct: ctrl+t cycles thinking level
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
        let msg = app.cycle_reasoning_effort();
        cmd_tx.send(msg)?;
        app.set_status(
            app::LogLevel::Info,
            "model",
            format!("thinking: {}", app.reasoning_effort_label()),
        );
        save_cache(app);
        return Ok(AppAction::None);
    }

    // chord start: ctrl+x
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('x') {
        app.chord = true;
        app.set_status(app::LogLevel::Debug, "input", "C-x ...");
        return Ok(AppAction::None);
    }

    // popup handling
    match app.popup {
        Popup::ModelSelect => {
            handle_model_popup_key(app, key, cmd_tx)?;
            return Ok(AppAction::None);
        }
        Popup::SessionSelect => {
            handle_session_popup_key(app, key, cmd_tx)?;
            return Ok(AppAction::None);
        }
        Popup::NewSession => {
            handle_new_session_popup_key(app, key, cmd_tx)?;
            return Ok(AppAction::None);
        }
        Popup::ThemeSelect => {
            handle_theme_popup_key(app, key)?;
            return Ok(AppAction::None);
        }
        Popup::Help => {
            match key.code {
                KeyCode::Esc => {
                    app.popup = Popup::None;
                }
                KeyCode::Up => {
                    app.help_scroll = app.help_scroll.saturating_sub(1);
                }
                KeyCode::Down => {
                    app.help_scroll = app.help_scroll.saturating_add(1);
                }
                _ => {}
            }
            return Ok(AppAction::None);
        }
        Popup::Log => {
            handle_log_popup_key(app, key)?;
            return Ok(AppAction::None);
        }
        Popup::None => {}
    }

    // global: tab toggles mode when no popup is active
    if key.code == KeyCode::Tab {
        if !can_send_server_commands(app) {
            return Ok(AppAction::None);
        }
        app.cache_session_mode_state();

        let next = app.next_mode().to_string();
        cmd_tx.send(ClientMsg::SetAgentMode { mode: next.clone() })?;
        if let (Some(provider), Some(model)) =
            (app.current_provider.clone(), app.current_model.clone())
        {
            let outgoing_mode = app.agent_mode.clone();
            app.set_mode_model_preference(&outgoing_mode, &provider, &model);
        }
        app.agent_mode = next;

        for msg in app.apply_cached_mode_state() {
            cmd_tx.send(msg)?;
        }
        if !app
            .session_cache
            .get(app.session_id.as_deref().unwrap_or(""))
            .is_some_and(|modes| modes.contains_key(&app.agent_mode))
        {
            apply_mode_model_if_preferred(app, cmd_tx)?;
        }

        save_config(app);
        save_cache(app);
        return Ok(AppAction::None);
    }

    match app.screen {
        Screen::Sessions => handle_sessions_key(app, key, cmd_tx)?,
        Screen::Chat => handle_chat_key(app, key, cmd_tx)?,
    }
    Ok(AppAction::None)
}

/// Persist current app state to `~/.qmt/tui.toml`.  Called at every
/// user-initiated change that should survive a restart.
pub(crate) fn save_config(app: &App) {
    let merged = config::TuiConfig::load().with_app_settings(app);
    merged.save();
}

/// Persist session effort cache to `~/.cache/qmt/tui-cache.toml`.
pub(crate) fn save_cache(app: &App) {
    config::TuiCache::from_app(app).save();
}

/// Handle second key of a ctrl+x chord. Works in any screen.
pub(crate) fn handle_chord(
    app: &mut App,
    key: KeyEvent,
    cmd_tx: &mpsc::UnboundedSender<ClientMsg>,
) -> anyhow::Result<()> {
    match key.code {
        KeyCode::Char('m') => {
            app.popup = Popup::ModelSelect;
            app.model_cursor = 0;
            app.model_filter.clear();
        }
        KeyCode::Char('n') => {
            if !can_send_server_commands(app) {
                return Ok(());
            }
            app.open_new_session_popup();
        }
        KeyCode::Char('q') => {
            app.should_quit = true;
        }
        KeyCode::Char('e') => {
            app.set_status(
                app::LogLevel::Warn,
                "editor",
                "external editor unavailable here",
            );
        }
        KeyCode::Char('s') => {
            if !can_send_server_commands(app) {
                return Ok(());
            }
            app.popup = Popup::SessionSelect;
            app.session_cursor = 0;
            app.session_filter.clear();
            cmd_tx.send(ClientMsg::ListSessions)?;
        }

        KeyCode::Char('t') => {
            app.popup = Popup::ThemeSelect;
            app.theme_cursor = 0;
            app.theme_filter.clear();
        }
        KeyCode::Char('l') => {
            app.popup = Popup::Log;
            app.log_cursor = app.filtered_logs().len().saturating_sub(1);
            app.log_filter.clear();
        }
        KeyCode::Char('?') => {
            app.popup = Popup::Help;
            app.help_scroll = 0;
        }
        KeyCode::Char('u') => {
            if !can_send_server_commands(app) {
                return Ok(());
            }
            if app.is_turn_active() {
                app.set_status(
                    app::LogLevel::Warn,
                    "session",
                    "cannot undo while agent is active",
                );
            } else if app.has_pending_session_op() || app.has_pending_undo() {
                app.set_status(app::LogLevel::Warn, "session", "undo already pending");
            } else if let Some(turn) = app.current_undo_target().cloned() {
                if app.input.trim().is_empty() && !turn.text.is_empty() {
                    app.input = turn.text.clone();
                    app.input_cursor = app.input.len();
                    app.input_scroll = 0;
                }
                app.push_pending_undo(&turn);
                app.activity = ActivityState::SessionOp(app::SessionOp::Undo);
                app.set_status(app::LogLevel::Info, "session", "undoing...");
                cmd_tx.send(ClientMsg::Undo {
                    message_id: turn.message_id,
                })?;
            } else {
                app.set_status(app::LogLevel::Warn, "session", "nothing to undo");
            }
        }
        KeyCode::Char('r') => {
            if !can_send_server_commands(app) {
                return Ok(());
            }
            if app.is_turn_active() {
                app.set_status(
                    app::LogLevel::Warn,
                    "session",
                    "cannot redo while agent is active",
                );
            } else if app.has_pending_session_op() || app.has_pending_undo() {
                app.set_status(app::LogLevel::Warn, "session", "undo already pending");
            } else if app.can_redo() {
                app.activity = ActivityState::SessionOp(app::SessionOp::Redo);
                app.set_status(app::LogLevel::Info, "session", "redoing...");
                cmd_tx.send(ClientMsg::Redo)?;
            } else {
                app.set_status(app::LogLevel::Warn, "session", "nothing to redo");
            }
        }
        _ => {
            app.set_status(app::LogLevel::Debug, "input", "unknown chord");
        }
    }
    Ok(())
}

pub(crate) fn handle_sessions_key(
    app: &mut App,
    key: KeyEvent,
    cmd_tx: &mpsc::UnboundedSender<ClientMsg>,
) -> anyhow::Result<()> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.should_quit = true;
            return Ok(());
        }
        _ => {}
    }

    match apply_sessions_key(app, key.code) {
        SessionKeyAction::LoadSession { session_id } => {
            cmd_tx.send(ClientMsg::LoadSession {
                session_id: session_id.clone(),
            })?;
            cmd_tx.send(ClientMsg::SubscribeSession {
                session_id,
                agent_id: app.agent_id.clone(),
            })?;
        }
        SessionKeyAction::DeleteSession { session_id } => {
            cmd_tx.send(ClientMsg::DeleteSession { session_id })?;
        }
        SessionKeyAction::NewSession => {
            app.open_new_session_popup();
        }
        SessionKeyAction::None => {}
    }
    Ok(())
}

pub(crate) fn handle_session_popup_key(
    app: &mut App,
    key: KeyEvent,
    cmd_tx: &mpsc::UnboundedSender<ClientMsg>,
) -> anyhow::Result<()> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
        if !can_send_server_commands(app) {
            return Ok(());
        }
        app.open_new_session_popup();
        return Ok(());
    }

    match apply_popup_session_key(app, key.code) {
        SessionKeyAction::LoadSession { session_id } => {
            cmd_tx.send(ClientMsg::LoadSession {
                session_id: session_id.clone(),
            })?;
            cmd_tx.send(ClientMsg::SubscribeSession {
                session_id,
                agent_id: app.agent_id.clone(),
            })?;
        }
        SessionKeyAction::DeleteSession { session_id } => {
            cmd_tx.send(ClientMsg::DeleteSession { session_id })?;
        }
        SessionKeyAction::NewSession | SessionKeyAction::None => {}
    }
    Ok(())
}

/// Pure key-handler for the session popup. Returns a [`SessionKeyAction`] that
/// the caller should forward to the server.
///
/// Uses [`App::visible_popup_items`] (grouped, no caps) and
/// [`App::popup_collapsed_groups`] so its collapse state is fully independent
/// of the start-page.
pub(crate) fn apply_popup_session_key(
    app: &mut App,
    key: crossterm::event::KeyCode,
) -> SessionKeyAction {
    use crate::app::PopupItem;

    match key {
        KeyCode::Esc => {
            app.popup = Popup::None;
        }
        KeyCode::Up => {
            app.session_cursor = app.session_cursor.saturating_sub(1);
        }
        KeyCode::Down => {
            let max = app.visible_popup_items().len().saturating_sub(1);
            app.session_cursor = (app.session_cursor + 1).min(max);
        }
        KeyCode::Enter => {
            let items = app.visible_popup_items();
            if let Some(item) = items.get(app.session_cursor).cloned() {
                match item {
                    PopupItem::GroupHeader { cwd, .. } => {
                        app.toggle_popup_group_collapse(cwd.as_deref());
                        // Clamp cursor: collapsing may hide rows the cursor pointed at.
                        let new_len = app.visible_popup_items().len();
                        if new_len > 0 && app.session_cursor >= new_len {
                            app.session_cursor = new_len - 1;
                        }
                    }
                    PopupItem::Session {
                        group_idx,
                        session_idx,
                    } => {
                        let sid = app.session_groups[group_idx].sessions[session_idx]
                            .session_id
                            .clone();
                        app.popup = Popup::None;
                        return SessionKeyAction::LoadSession { session_id: sid };
                    }
                }
            }
        }
        KeyCode::Delete => {
            let items = app.visible_popup_items();
            if let Some(PopupItem::Session {
                group_idx,
                session_idx,
            }) = items.get(app.session_cursor).cloned()
            {
                let sid = app.session_groups[group_idx].sessions[session_idx]
                    .session_id
                    .clone();
                // Optimistic remove
                app.session_groups[group_idx].sessions.remove(session_idx);
                app.session_groups.retain(|g| !g.sessions.is_empty());
                let new_len = app.visible_popup_items().len();
                if new_len > 0 && app.session_cursor >= new_len {
                    app.session_cursor = new_len - 1;
                }
                return SessionKeyAction::DeleteSession { session_id: sid };
            }
            // Delete on a GroupHeader: no-op
        }
        KeyCode::Backspace => {
            app.session_filter.pop();
            app.session_cursor = 0;
        }
        KeyCode::Char(c) => {
            app.session_filter.push(c);
            app.session_cursor = 0;
        }
        _ => {}
    }
    SessionKeyAction::None
}

pub(crate) fn handle_log_popup_key(app: &mut App, key: KeyEvent) -> anyhow::Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.popup = Popup::None;
        }
        KeyCode::Up => {
            app.log_cursor = app.log_cursor.saturating_sub(1);
        }
        KeyCode::Down => {
            let max = app.filtered_logs().len().saturating_sub(1);
            app.log_cursor = (app.log_cursor + 1).min(max);
        }
        KeyCode::PageUp => {
            app.log_cursor = app.log_cursor.saturating_sub(10);
        }
        KeyCode::PageDown => {
            let max = app.filtered_logs().len().saturating_sub(1);
            app.log_cursor = (app.log_cursor + 10).min(max);
        }
        KeyCode::Home => {
            app.log_cursor = 0;
        }
        KeyCode::End => {
            app.log_cursor = app.filtered_logs().len().saturating_sub(1);
        }
        KeyCode::Backspace => {
            app.log_filter.pop();
            app.log_cursor = app.filtered_logs().len().saturating_sub(1);
        }
        KeyCode::Tab => {
            app.cycle_log_level_filter();
            app.log_cursor = app.filtered_logs().len().saturating_sub(1);
        }
        KeyCode::Char(c) => {
            app.log_filter.push(c);
            app.log_cursor = 0;
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn handle_new_session_popup_key(
    app: &mut App,
    key: KeyEvent,
    cmd_tx: &mpsc::UnboundedSender<ClientMsg>,
) -> anyhow::Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.popup = Popup::None;
        }
        KeyCode::Up => {
            app.move_new_session_completion_selection(-1);
        }
        KeyCode::Down => {
            app.move_new_session_completion_selection(1);
        }
        KeyCode::Tab => {
            app.accept_selected_new_session_completion();
        }
        KeyCode::Left => {
            app.new_session_cursor = app.new_session_cursor.saturating_sub(1);
            app.refresh_new_session_completion();
        }
        KeyCode::Right => {
            app.new_session_cursor = (app.new_session_cursor + 1).min(app.new_session_path.len());
            app.refresh_new_session_completion();
        }
        KeyCode::Home => {
            app.new_session_cursor = 0;
            app.refresh_new_session_completion();
        }
        KeyCode::End => {
            app.new_session_cursor = app.new_session_path.len();
            app.refresh_new_session_completion();
        }
        KeyCode::Backspace => {
            if app.new_session_cursor > 0 && !app.new_session_path.is_empty() {
                let idx = app.new_session_cursor - 1;
                app.new_session_path.remove(idx);
                app.new_session_cursor = idx;
            }
            app.refresh_new_session_completion();
        }
        KeyCode::Char(c) => {
            app.new_session_path.insert(app.new_session_cursor, c);
            app.new_session_cursor += 1;
            app.refresh_new_session_completion();
        }
        KeyCode::Enter => {
            if !can_send_server_commands(app) {
                return Ok(());
            }
            let cwd = app.normalize_new_session_path(&app.new_session_path);
            app.popup = Popup::None;
            cmd_tx.send(ClientMsg::NewSession {
                cwd,
                request_id: None,
            })?;
        }
        _ => {}
    }
    Ok(())
}

/// Invalidate every theme-dependent cache in `app` so that the next render
/// frame rebuilds styled lines with the current palette.
///
/// Covers:
/// - `card_cache` (finalized message cards)
/// - `streaming_cache` / `streaming_thinking_cache`
/// - `ToolDetail::Edit` / `ToolDetail::WriteFile` inline cached_lines
pub(crate) fn invalidate_theme_caches(app: &mut App) {
    app.card_cache.invalidate();
    app.streaming_cache.invalidate();
    app.streaming_thinking_cache.invalidate();

    // Re-generate diff/write preview lines baked into ToolCall entries.
    for entry in &mut app.messages {
        if let app::ChatEntry::ToolCall { detail, .. } = entry {
            match detail {
                app::ToolDetail::Edit {
                    old,
                    new,
                    start_line,
                    cached_lines,
                    ..
                } => {
                    *cached_lines = ui::build_diff_lines(old, new, *start_line);
                }
                app::ToolDetail::WriteFile {
                    content,
                    cached_lines,
                    ..
                } => {
                    *cached_lines = ui::build_write_lines(content);
                }
                _ => {}
            }
        }
    }
}

pub(crate) fn handle_theme_popup_key(app: &mut App, key: KeyEvent) -> anyhow::Result<()> {
    let filtered_len = || -> usize {
        let q = app.theme_filter.to_lowercase();
        if q.is_empty() {
            theme::Theme::available_themes().len()
        } else {
            theme::Theme::available_themes()
                .iter()
                .filter(|t| t.label.to_lowercase().contains(&q) || t.id.to_lowercase().contains(&q))
                .count()
        }
    };

    let filtered_index = |cursor: usize| -> Option<usize> {
        let q = app.theme_filter.to_lowercase();
        let iter = theme::Theme::available_themes().iter().enumerate();
        if q.is_empty() {
            Some(cursor)
        } else {
            iter.filter(|(_, t)| {
                t.label.to_lowercase().contains(&q) || t.id.to_lowercase().contains(&q)
            })
            .nth(cursor)
            .map(|(i, _)| i)
        }
    };

    match key.code {
        KeyCode::Esc => {
            app.popup = Popup::None;
        }
        KeyCode::Up => {
            app.theme_cursor = app.theme_cursor.saturating_sub(1);
        }
        KeyCode::Down => {
            let max = filtered_len().saturating_sub(1);
            app.theme_cursor = (app.theme_cursor + 1).min(max);
        }
        KeyCode::Enter => {
            if let Some(idx) = filtered_index(app.theme_cursor) {
                theme::Theme::set_by_index(idx);
                theme::Theme::begin_frame();
                invalidate_theme_caches(app);
                app.popup = Popup::None;
                save_config(app);
            }
        }
        KeyCode::Backspace => {
            app.theme_filter.pop();
            app.theme_cursor = 0;
        }
        KeyCode::Char(c) => {
            app.theme_filter.push(c);
            app.theme_cursor = 0;
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn handle_chat_key(
    app: &mut App,
    key: KeyEvent,
    cmd_tx: &mpsc::UnboundedSender<ClientMsg>,
) -> anyhow::Result<()> {
    if app.input_line_width == 0 {
        app.input_line_width = 1;
    }
    let input_blocked = app.input_blocked_by_activity();
    match key.code {
        KeyCode::Esc => {
            if app.mention_state.is_some() {
                app.mention_state = None;
                app.clear_cancel_confirm();
            } else if app.has_cancellable_activity() {
                if app.cancel_confirm_active() {
                    app.clear_cancel_confirm();
                    app.set_status(app::LogLevel::Warn, "activity", "stopping...");
                    cmd_tx.send(ClientMsg::CancelSession)?;
                } else {
                    app.arm_cancel_confirm();
                }
            } else {
                app.clear_cancel_confirm();
            }
        }
        KeyCode::Enter => {
            if app.mention_state.is_some() && app.accept_selected_mention() {
                if let Some(msg) = app.request_file_index_if_needed() {
                    cmd_tx.send(msg)?;
                }
                return Ok(());
            }
            if !app.input.is_empty() {
                if input_blocked || !can_send_server_commands(app) {
                    return Ok(());
                }
                let (text, links) = app.build_prompt_text_and_links(&app.input);
                let _ = app.take_input();
                let mut prompt = vec![PromptBlock::Text { text }];
                for path in links {
                    prompt.push(PromptBlock::ResourceLink {
                        name: path.clone(),
                        uri: path,
                    });
                }
                cmd_tx.send(ClientMsg::Prompt { prompt })?;
            }
        }
        KeyCode::Tab => {
            if !input_blocked
                && app.mention_state.is_some()
                && app.accept_selected_mention()
                && let Some(msg) = app.request_file_index_if_needed()
            {
                cmd_tx.send(msg)?;
            }
        }
        KeyCode::Char(c) => {
            if !input_blocked {
                app.input_insert(c);
                if let Some(msg) = app.request_file_index_if_needed() {
                    cmd_tx.send(msg)?;
                }
            }
        }
        KeyCode::Up => {
            if input_blocked {
                return Ok(());
            }
            if app.mention_state.is_some() {
                app.move_mention_selection(-1);
            } else {
                app.input_up_visual(2);
            }
        }
        KeyCode::Down => {
            if input_blocked {
                return Ok(());
            }
            if app.mention_state.is_some() {
                app.move_mention_selection(1);
            } else {
                app.input_down_visual(2);
            }
        }
        KeyCode::PageUp => {
            app.scroll_offset = app.scroll_offset.saturating_add(10);
        }
        KeyCode::PageDown => {
            app.scroll_offset = app.scroll_offset.saturating_sub(10);
        }
        KeyCode::Backspace => {
            if !input_blocked {
                app.input_backspace();
            }
        }
        KeyCode::Delete => {
            if !input_blocked {
                app.input_delete();
            }
        }
        KeyCode::Left => {
            if !input_blocked {
                app.input_left();
            }
        }
        KeyCode::Right => {
            if !input_blocked {
                app.input_right();
            }
        }
        KeyCode::Home => {
            if !input_blocked {
                app.input_home();
            }
        }
        KeyCode::End => {
            if input_blocked {
                app.scroll_offset = 0;
            } else if app.input.is_empty() {
                app.scroll_offset = 0; // snap to bottom
            } else {
                app.input_end();
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn handle_model_popup_key(
    app: &mut App,
    key: KeyEvent,
    cmd_tx: &mpsc::UnboundedSender<ClientMsg>,
) -> anyhow::Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.popup = Popup::None;
        }
        KeyCode::Up => {
            app.model_cursor = app.model_cursor.saturating_sub(1);
        }
        KeyCode::Down => {
            let max = app.filtered_models().len().saturating_sub(1);
            app.model_cursor = (app.model_cursor + 1).min(max);
        }
        KeyCode::Enter => {
            let selected: Option<protocol::ModelEntry> = app
                .filtered_models()
                .get(app.model_cursor)
                .cloned()
                .cloned();
            if let Some(model) = selected {
                if let Some(sid) = app.session_id.clone() {
                    cmd_tx.send(ClientMsg::SetSessionModel {
                        session_id: sid,
                        model_id: model.id.clone(),
                        node_id: model.node_id.clone(),
                    })?;
                }
                app.current_model = Some(model.model.clone());
                app.current_provider = Some(model.provider.clone());
                // Record this model as the preference for the current agent mode so
                // it is automatically re-applied whenever the user switches back to
                // this mode later in the same TUI session.
                app.set_mode_model_preference(
                    &app.agent_mode.clone(),
                    &model.provider,
                    &model.model,
                );
                // Drop reasoning effort to auto when switching model.
                if app.reasoning_effort.is_some() {
                    app.reasoning_effort = None;
                    cmd_tx.send(ClientMsg::SetReasoningEffort {
                        reasoning_effort: "auto".into(),
                    })?;
                }
                // Cache the new model + auto effort for this session + mode.
                app.cache_session_mode_state();
                app.popup = Popup::None;
                app.set_status(
                    app::LogLevel::Info,
                    "model",
                    format!("model: {}", model.label),
                );
                save_config(app);
                save_cache(app);
            }
        }
        KeyCode::Backspace => {
            app.model_filter.pop();
            app.model_cursor = 0;
        }
        KeyCode::Char(c) => {
            app.model_filter.push(c);
            app.model_cursor = 0;
        }
        _ => {}
    }
    Ok(())
}

// ── Pure key logic for the sessions screen ────────────────────────────────────
//
// `apply_sessions_key` returns the `ClientMsg`(s) that should be sent to the
// server (if any).  Keeping the mutation separate from the channel send makes
// it fully unit-testable without a real channel.

/// Result of handling a sessions-screen key.
#[derive(Debug, PartialEq)]
pub(crate) enum SessionKeyAction {
    /// Nothing to send to the server.
    None,
    /// Load the session with the given id and subscribe to it.
    LoadSession { session_id: String },
    /// Delete the session with the given id.
    DeleteSession { session_id: String },
    /// Create a new session.
    NewSession,
}

/// Apply a key event on the sessions screen, mutate `app`, and return the
/// action (if any) that the caller should forward to the server.
pub(crate) fn apply_sessions_key(
    app: &mut App,
    key: crossterm::event::KeyCode,
) -> SessionKeyAction {
    use crate::app::StartPageItem;

    match key {
        KeyCode::Up => {
            app.session_cursor = app.session_cursor.saturating_sub(1);
            // Adjust scroll if needed (draw_start also does this, but keeping
            // state consistent here means tests don't need a frame).
            if app.session_cursor < app.start_page_scroll {
                app.start_page_scroll = app.session_cursor;
            }
        }
        KeyCode::Down => {
            // Button slot is one past the last item (items.len()).
            let max = app.visible_start_items().len(); // inclusive: button slot
            if app.session_cursor < max {
                app.session_cursor += 1;
            }
        }
        KeyCode::Enter => {
            let items = app.visible_start_items();
            // Cursor on the button slot (one past the last item)?
            if app.session_cursor == items.len() {
                return SessionKeyAction::NewSession;
            }
            if let Some(item) = items.get(app.session_cursor).cloned() {
                match item {
                    StartPageItem::GroupHeader { cwd, .. } => {
                        app.toggle_group_collapse(cwd.as_deref());
                        // Clamp cursor after collapse may hide rows.
                        let new_len = app.visible_start_items().len();
                        if new_len > 0 && app.session_cursor >= new_len {
                            app.session_cursor = new_len - 1;
                        }
                    }
                    StartPageItem::Session {
                        group_idx,
                        session_idx,
                    } => {
                        let sid = app.session_groups[group_idx].sessions[session_idx]
                            .session_id
                            .clone();
                        return SessionKeyAction::LoadSession { session_id: sid };
                    }
                    StartPageItem::ShowMore { .. } => {
                        app.popup = Popup::SessionSelect;
                        app.session_cursor = 0;
                        app.session_filter.clear();
                    }
                }
            }
        }
        KeyCode::Delete => {
            let items = app.visible_start_items();
            if let Some(StartPageItem::Session {
                group_idx,
                session_idx,
            }) = items.get(app.session_cursor).cloned()
            {
                let sid = app.session_groups[group_idx].sessions[session_idx]
                    .session_id
                    .clone();
                // Optimistic remove
                app.session_groups[group_idx].sessions.remove(session_idx);
                app.session_groups.retain(|g| !g.sessions.is_empty());
                let new_len = app.visible_start_items().len();
                if new_len > 0 && app.session_cursor >= new_len {
                    app.session_cursor = new_len - 1;
                }
                return SessionKeyAction::DeleteSession { session_id: sid };
            }
            // Delete on a GroupHeader: no-op
        }
        KeyCode::Backspace => {
            app.session_filter.pop();
            app.session_cursor = 0;
            app.start_page_scroll = 0;
        }
        KeyCode::Char(c) => {
            app.session_filter.push(c);
            app.session_cursor = 0;
            app.start_page_scroll = 0;
        }
        _ => {}
    }
    SessionKeyAction::None
}
