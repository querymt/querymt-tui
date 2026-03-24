#![allow(dead_code)]

mod app;
mod config;
mod highlight;
mod markdown;
mod protocol;
mod theme;
mod themes_gen;
mod ui;

use std::time::Duration;

use app::{App, Popup, Screen};
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::{SinkExt, StreamExt};
use protocol::{ClientMsg, PromptBlock, RawServerMsg};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug)]
enum ConnectionManagerEvent {
    State(app::ConnectionEvent),
}

fn reconnect_delay_ms(attempt: u32) -> u64 {
    let capped = attempt.min(5);
    250 * (1u64 << capped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use app::{
        ChatEntry, ElicitationField, ElicitationFieldKind, ElicitationOption, ElicitationState,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc;
    use ui::OUTCOME_BULLET;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn make_elicitation_single_select() -> ElicitationState {
        ElicitationState::new_for_test(vec![ElicitationField {
            name: "choice".into(),
            title: "Pick one".into(),
            description: None,
            required: true,
            kind: ElicitationFieldKind::SingleSelect {
                options: vec![
                    ElicitationOption {
                        value: serde_json::json!("a"),
                        label: "Alpha".into(),
                        description: None,
                    },
                    ElicitationOption {
                        value: serde_json::json!("b"),
                        label: "Beta".into(),
                        description: None,
                    },
                ],
            },
        }])
    }

    fn make_app_with_elicitation(state: ElicitationState) -> App {
        let mut app = App::new();
        app.conn = app::ConnState::Connected;
        app.session_id = Some("sess-1".into());
        app.messages.push(ChatEntry::Elicitation {
            elicitation_id: state.elicitation_id.clone(),
            message: state.message.clone(),
            source: state.source.clone(),
            outcome: None,
        });
        app.elicitation = Some(state);
        app
    }

    #[test]
    fn reconnect_delay_caps_after_five_steps() {
        assert_eq!(reconnect_delay_ms(0), 250);
        assert_eq!(reconnect_delay_ms(1), 500);
        assert_eq!(reconnect_delay_ms(2), 1000);
        assert_eq!(reconnect_delay_ms(3), 2000);
        assert_eq!(reconnect_delay_ms(4), 4000);
        assert_eq!(reconnect_delay_ms(5), 8000);
        assert_eq!(reconnect_delay_ms(8), 8000);
    }

    // ── Elicitation key handling ──────────────────────────────────────────────

    #[test]
    fn elicitation_down_moves_option_cursor() {
        let mut app = make_app_with_elicitation(make_elicitation_single_select());
        let (tx, _rx) = mpsc::unbounded_channel();
        handle_elicitation_key(&mut app, key(KeyCode::Down), &tx).unwrap();
        assert_eq!(app.elicitation.as_ref().unwrap().option_cursor, 1);
    }

    #[test]
    fn elicitation_up_does_not_go_below_zero() {
        let mut app = make_app_with_elicitation(make_elicitation_single_select());
        let (tx, _rx) = mpsc::unbounded_channel();
        handle_elicitation_key(&mut app, key(KeyCode::Up), &tx).unwrap();
        assert_eq!(app.elicitation.as_ref().unwrap().option_cursor, 0);
    }

    #[test]
    fn elicitation_enter_on_single_select_sends_accept_and_resolves() {
        let mut app = make_app_with_elicitation(make_elicitation_single_select());
        let (tx, mut rx) = mpsc::unbounded_channel();
        // Move to Beta and press Enter
        handle_elicitation_key(&mut app, key(KeyCode::Down), &tx).unwrap();
        handle_elicitation_key(&mut app, key(KeyCode::Enter), &tx).unwrap();

        // Elicitation should be cleared
        assert!(app.elicitation.is_none());

        // Accept response sent
        let msg = rx.try_recv().expect("message sent");
        assert!(matches!(msg,
            ClientMsg::ElicitationResponse { action, content: Some(ref c), .. }
            if action == "accept" && c["choice"] == "b"
        ));

        // Chat card updated with the selected label
        assert!(app.messages.iter().any(|m| matches!(m,
            ChatEntry::Elicitation { outcome: Some(o), .. } if *o == format!("{OUTCOME_BULLET}Beta")
        )));
    }

    #[test]
    fn elicitation_esc_sends_decline_and_resolves() {
        let mut app = make_app_with_elicitation(make_elicitation_single_select());
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_elicitation_key(&mut app, key(KeyCode::Esc), &tx).unwrap();

        assert!(app.elicitation.is_none());
        let msg = rx.try_recv().expect("message sent");
        assert!(matches!(msg,
            ClientMsg::ElicitationResponse { action, .. } if action == "decline"
        ));
        assert!(app.messages.iter().any(|m| matches!(m,
            ChatEntry::Elicitation { outcome: Some(o), .. } if o == "declined"
        )));
    }

    #[test]
    fn elicitation_enter_on_text_field_sends_accept_with_text() {
        let mut app =
            make_app_with_elicitation(ElicitationState::new_for_test(vec![ElicitationField {
                name: "name".into(),
                title: "Name".into(),
                description: None,
                required: true,
                kind: ElicitationFieldKind::TextInput,
            }]));
        app.elicitation.as_mut().unwrap().text_input = "Alice".into();
        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_elicitation_key(&mut app, key(KeyCode::Enter), &tx).unwrap();

        assert!(app.elicitation.is_none());
        let msg = rx.try_recv().expect("message sent");
        assert!(matches!(msg,
            ClientMsg::ElicitationResponse { action, content: Some(ref c), .. }
            if action == "accept" && c["name"] == "Alice"
        ));
        assert!(app.messages.iter().any(|m| matches!(m,
            ChatEntry::Elicitation { outcome: Some(o), .. } if o == "Alice"
        )));
    }

    #[test]
    fn elicitation_char_input_appends_to_text_buffer() {
        let mut app =
            make_app_with_elicitation(ElicitationState::new_for_test(vec![ElicitationField {
                name: "msg".into(),
                title: "Message".into(),
                description: None,
                required: false,
                kind: ElicitationFieldKind::TextInput,
            }]));
        let (tx, _rx) = mpsc::unbounded_channel();
        handle_elicitation_key(&mut app, key(KeyCode::Char('H')), &tx).unwrap();
        handle_elicitation_key(&mut app, key(KeyCode::Char('i')), &tx).unwrap();
        assert_eq!(app.elicitation.as_ref().unwrap().text_input, "Hi");
    }

    #[test]
    fn elicitation_backspace_removes_last_char_from_text_buffer() {
        let mut app =
            make_app_with_elicitation(ElicitationState::new_for_test(vec![ElicitationField {
                name: "msg".into(),
                title: "Message".into(),
                description: None,
                required: false,
                kind: ElicitationFieldKind::TextInput,
            }]));
        app.elicitation.as_mut().unwrap().text_input = "Hi".into();
        app.elicitation.as_mut().unwrap().text_cursor = 2;
        let (tx, _rx) = mpsc::unbounded_channel();
        handle_elicitation_key(&mut app, key(KeyCode::Backspace), &tx).unwrap();
        assert_eq!(app.elicitation.as_ref().unwrap().text_input, "H");
    }
}

#[derive(Parser)]
#[command(name = "qmt-tui", about = "querymt terminal interface")]
struct Cli {
    /// Server address (e.g. 127.0.0.1:3030). Overrides the value in ~/.qmt/tui.toml.
    #[arg(long)]
    server: Option<String>,
}

fn detect_launch_cwd() -> Option<String> {
    std::env::current_dir()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Load persistent config; CLI args override config defaults.
    let cfg = config::TuiConfig::load();

    let addr = cli
        .server
        .or_else(|| cfg.server.addr.clone())
        .unwrap_or_else(|| "127.0.0.1:3030".to_string());
    let tls = cfg.server.tls.unwrap_or(false);

    // Apply saved theme (falls back to built-in default if absent or unknown).
    let theme_id = cfg.theme.as_deref().unwrap_or("base16-querymate");
    theme::Theme::init(theme_id);

    let scheme = if tls { "wss" } else { "ws" };
    let url = format!("{scheme}://{addr}/ui/ws");

    // channels for the event loop
    let (srv_tx, mut srv_rx) = mpsc::unbounded_channel::<RawServerMsg>();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let (conn_tx, mut conn_rx) = mpsc::unbounded_channel::<ConnectionManagerEvent>();

    tokio::spawn(connection_manager(url, srv_tx, cmd_rx, conn_tx));

    // setup terminal
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    app.launch_cwd = detect_launch_cwd();
    // Hydrate session effort cache from disk.
    config::TuiCache::load().hydrate_app(&mut app);
    let result = run_loop(&mut terminal, &mut app, &mut srv_rx, &mut conn_rx, &cmd_tx).await;

    // restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn connection_manager(
    url: String,
    srv_tx: mpsc::UnboundedSender<RawServerMsg>,
    mut cmd_rx: mpsc::UnboundedReceiver<ClientMsg>,
    conn_tx: mpsc::UnboundedSender<ConnectionManagerEvent>,
) {
    let mut attempt = 0u32;

    loop {
        if attempt > 0 {
            let delay_ms = reconnect_delay_ms(attempt - 1);
            let _ = conn_tx.send(ConnectionManagerEvent::State(
                app::ConnectionEvent::Connecting { attempt, delay_ms },
            ));
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        match tokio_tungstenite::connect_async(&url).await {
            Ok((ws_stream, _)) => {
                let _ = conn_tx.send(ConnectionManagerEvent::State(
                    app::ConnectionEvent::Connected,
                ));
                let (mut ws_tx, mut ws_rx) = ws_stream.split();

                let disconnected_reason = loop {
                    tokio::select! {
                        biased;
                        maybe_cmd = cmd_rx.recv() => {
                            let Some(cmd) = maybe_cmd else { return; };
                            if let Ok(json) = serde_json::to_string(&cmd)
                                && ws_tx.send(Message::Text(json.into())).await.is_err()
                            {
                                break String::from("send failed");
                            }
                        }
                        maybe_msg = ws_rx.next() => {
                            match maybe_msg {
                                Some(Ok(Message::Text(text))) => {
                                    if let Ok(raw) = serde_json::from_str::<RawServerMsg>(&text) {
                                        let _ = srv_tx.send(raw);
                                    }
                                }
                                Some(Ok(_)) => {}
                                Some(Err(err)) => {
                                    break err.to_string();
                                }
                                None => {
                                    break String::from("socket closed");
                                }
                            }
                        }
                    }
                };

                let _ = conn_tx.send(ConnectionManagerEvent::State(
                    app::ConnectionEvent::Disconnected {
                        reason: disconnected_reason,
                    },
                ));
                attempt = 1;
            }
            Err(err) => {
                attempt = attempt.saturating_add(1).max(1);
                let _ = conn_tx.send(ConnectionManagerEvent::State(
                    app::ConnectionEvent::Disconnected {
                        reason: err.to_string(),
                    },
                ));
            }
        }
    }
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    srv_rx: &mut mpsc::UnboundedReceiver<RawServerMsg>,
    conn_rx: &mut mpsc::UnboundedReceiver<ConnectionManagerEvent>,
    cmd_tx: &mpsc::UnboundedSender<ClientMsg>,
) -> anyhow::Result<()> {
    loop {
        app.tick = app.tick.wrapping_add(1);
        app.clear_expired_cancel_confirm();
        terminal.draw(|f| ui::draw(f, app))?;

        // poll for terminal events or server messages
        tokio::select! {
            biased;
            Some(event) = conn_rx.recv() => {
                match event {
                    ConnectionManagerEvent::State(state) => {
                        let was_connected = app.conn == app::ConnState::Connected;
                        app.handle_connection_event(state);
                        if app.conn == app::ConnState::Connected {
                            cmd_tx.send(ClientMsg::Init)?;
                            cmd_tx.send(ClientMsg::ListAllModels { refresh: false })?;
                            cmd_tx.send(ClientMsg::GetAgentMode)?;
                            if let Some(session_id) = app.session_id.clone() {
                                cmd_tx.send(ClientMsg::LoadSession {
                                    session_id: session_id.clone(),
                                })?;
                                cmd_tx.send(ClientMsg::SubscribeSession {
                                    session_id,
                                    agent_id: app.agent_id.clone(),
                                })?;
                            }
                        } else if was_connected && app.conn == app::ConnState::Disconnected {
                            app.status = "connection lost - reconnecting...".into();
                        }
                    }
                }
            }
            // server messages
            Some(msg) = srv_rx.recv() => {
                // Save config when the server authoritatively updates effort.
                let is_effort_push = msg.msg_type == "reasoning_effort";
                for reply in app.handle_server_msg(msg) {
                    // if reloading session, also re-subscribe
                    if let ClientMsg::LoadSession { ref session_id } = reply {
                        let sid = session_id.clone();
                        cmd_tx.send(reply)?;
                        cmd_tx.send(ClientMsg::SubscribeSession {
                            session_id: sid,
                            agent_id: app.agent_id.clone(),
                        })?;
                    } else {
                        cmd_tx.send(reply)?;
                    }
                }
                if is_effort_push {
                    save_cache(app);
                }
            }
            // terminal input
            _ = tokio::task::spawn_blocking(|| {
                event::poll(Duration::from_millis(50)).unwrap_or(false)
            }) => {
                if event::poll(Duration::from_millis(0))?
                    && let Event::Key(key) = event::read()?
                {
                    handle_key(app, key, cmd_tx)?;
                }
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn can_send_server_commands(app: &mut App) -> bool {
    if app.conn == app::ConnState::Connected {
        true
    } else {
        app.status = "not connected - waiting to reconnect".into();
        false
    }
}

/// Handle all keyboard input while an elicitation popup is active.
///
/// Returns `Ok(())` in all cases; the caller should return immediately after
/// this to avoid routing the key to the normal chat handler.
fn handle_elicitation_key(
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
fn apply_mode_model_if_preferred(
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
        app.status = format!("mode: {} → model: {}", app.agent_mode, entry.label);
        cmd_tx.send(ClientMsg::SetSessionModel {
            session_id: sid,
            model_id: entry.id,
            node_id: entry.node_id,
        })?;
    }
    Ok(())
}

fn handle_key(
    app: &mut App,
    key: KeyEvent,
    cmd_tx: &mpsc::UnboundedSender<ClientMsg>,
) -> anyhow::Result<()> {
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
        return Ok(());
    }

    // chord second key: ctrl+x was pressed, now handle the follow-up
    if app.chord {
        app.chord = false;
        app.status = "ready".into();
        return handle_chord(app, key, cmd_tx);
    }

    // elicitation popup takes full control of input when active
    if app.elicitation.is_some() {
        handle_elicitation_key(app, key, cmd_tx)?;
        return Ok(());
    }

    // direct: ctrl+t cycles thinking level
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
        let msg = app.cycle_reasoning_effort();
        cmd_tx.send(msg)?;
        app.status = format!("thinking: {}", app.reasoning_effort_label());
        save_cache(app);
        return Ok(());
    }

    // chord start: ctrl+x
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('x') {
        app.chord = true;
        app.status = "C-x ...".into();
        return Ok(());
    }

    // popup handling
    match app.popup {
        Popup::ModelSelect => {
            handle_model_popup_key(app, key, cmd_tx)?;
            return Ok(());
        }
        Popup::SessionSelect => {
            handle_session_popup_key(app, key, cmd_tx)?;
            return Ok(());
        }
        Popup::NewSession => {
            handle_new_session_popup_key(app, key, cmd_tx)?;
            return Ok(());
        }
        Popup::ThemeSelect => {
            handle_theme_popup_key(app, key)?;
            return Ok(());
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
            return Ok(());
        }
        Popup::None => {}
    }

    // global: tab toggles mode when no popup is active
    if key.code == KeyCode::Tab {
        if !can_send_server_commands(app) {
            return Ok(());
        }
        // Save outgoing mode state (model + effort) to session cache.
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

        // Restore incoming mode state (model + effort) from session cache.
        for msg in app.apply_cached_mode_state() {
            cmd_tx.send(msg)?;
        }
        // If no cache entry existed, fall back to mode_model_preferences.
        if !app
            .session_cache
            .get(app.session_id.as_deref().unwrap_or(""))
            .is_some_and(|modes| modes.contains_key(&app.agent_mode))
        {
            apply_mode_model_if_preferred(app, cmd_tx)?;
        }

        save_config(app);
        save_cache(app);
        return Ok(());
    }

    match app.screen {
        Screen::Sessions => handle_sessions_key(app, key, cmd_tx)?,
        Screen::Chat => handle_chat_key(app, key, cmd_tx)?,
    }
    Ok(())
}

/// Persist current app state to `~/.qmt/tui.toml`.  Called at every
/// user-initiated change that should survive a restart.
fn save_config(app: &App) {
    config::TuiConfig::from_app(app).save();
}

/// Persist session effort cache to `~/.cache/qmt/tui-cache.toml`.
fn save_cache(app: &App) {
    config::TuiCache::from_app(app).save();
}

/// Handle second key of a ctrl+x chord. Works in any screen.
fn handle_chord(
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
        KeyCode::Char('?') => {
            app.popup = Popup::Help;
            app.help_scroll = 0;
        }
        KeyCode::Char('u') => {
            if !can_send_server_commands(app) {
                return Ok(());
            }
            if app.is_thinking {
                app.status = "cannot undo while thinking".into();
            } else if app.pending_session_op.is_some() || app.has_pending_undo() {
                app.status = "undo already pending".into();
            } else if let Some(turn) = app.current_undo_target().cloned() {
                if app.input.trim().is_empty() && !turn.text.is_empty() {
                    app.input = turn.text.clone();
                    app.input_cursor = app.input.len();
                    app.input_scroll = 0;
                }
                app.push_pending_undo(&turn);
                app.pending_session_op = Some(app::SessionOp::Undo);
                app.status = "undoing...".into();
                cmd_tx.send(ClientMsg::Undo {
                    message_id: turn.message_id,
                })?;
            } else {
                app.status = "nothing to undo".into();
            }
        }
        KeyCode::Char('r') => {
            if !can_send_server_commands(app) {
                return Ok(());
            }
            if app.is_thinking {
                app.status = "cannot redo while thinking".into();
            } else if app.pending_session_op.is_some() || app.has_pending_undo() {
                app.status = "undo already pending".into();
            } else if app.can_redo() {
                app.pending_session_op = Some(app::SessionOp::Redo);
                app.status = "redoing...".into();
                cmd_tx.send(ClientMsg::Redo)?;
            } else {
                app.status = "nothing to redo".into();
            }
        }
        _ => {
            app.status = "unknown chord".into();
        }
    }
    Ok(())
}

fn handle_sessions_key(
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

fn handle_session_popup_key(
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

fn handle_new_session_popup_key(
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

fn handle_theme_popup_key(app: &mut App, key: KeyEvent) -> anyhow::Result<()> {
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

fn handle_chat_key(
    app: &mut App,
    key: KeyEvent,
    cmd_tx: &mpsc::UnboundedSender<ClientMsg>,
) -> anyhow::Result<()> {
    match key.code {
        KeyCode::Esc => {
            if app.mention_state.is_some() {
                app.mention_state = None;
                app.clear_cancel_confirm();
            } else if app.is_thinking {
                if app.cancel_confirm_active() {
                    app.clear_cancel_confirm();
                    app.status = "stopping...".into();
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
            if !app.input.is_empty() && !app.is_busy_for_input() {
                if !can_send_server_commands(app) {
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
            if app.mention_state.is_some()
                && app.accept_selected_mention()
                && let Some(msg) = app.request_file_index_if_needed()
            {
                cmd_tx.send(msg)?;
            }
        }
        KeyCode::Char(c) => {
            if !app.is_busy_for_input() {
                app.input_insert(c);
                if let Some(msg) = app.request_file_index_if_needed() {
                    cmd_tx.send(msg)?;
                }
            }
        }
        KeyCode::Up => {
            if app.mention_state.is_some() {
                app.move_mention_selection(-1);
            } else {
                app.scroll_offset = app.scroll_offset.saturating_add(3);
            }
        }
        KeyCode::Down => {
            if app.mention_state.is_some() {
                app.move_mention_selection(1);
            } else {
                app.scroll_offset = app.scroll_offset.saturating_sub(3);
            }
        }
        KeyCode::PageUp => {
            app.scroll_offset = app.scroll_offset.saturating_add(10);
        }
        KeyCode::PageDown => {
            app.scroll_offset = app.scroll_offset.saturating_sub(10);
        }
        KeyCode::Backspace => app.input_backspace(),
        KeyCode::Delete => app.input_delete(),
        KeyCode::Left => app.input_left(),
        KeyCode::Right => app.input_right(),
        KeyCode::Home => app.input_home(),
        KeyCode::End => {
            if app.input.is_empty() {
                app.scroll_offset = 0; // snap to bottom
            } else {
                app.input_end();
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_model_popup_key(
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
                app.status = format!("model: {}", model.label);
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

#[cfg(test)]
fn install_temp_persistence_paths(label: &str) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("qmt-main-tests-{label}-{pid}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    config::test_set_config_path_override(Some(dir.join("tui.toml")));
    config::test_set_cache_path_override(Some(dir.join("tui-cache.toml")));
}

#[cfg(test)]
fn clear_temp_persistence_paths() {
    config::test_set_config_path_override(None);
    config::test_set_cache_path_override(None);
}

#[cfg(test)]
struct PersistenceGuard;

#[cfg(test)]
impl PersistenceGuard {
    fn new(label: &str) -> Self {
        install_temp_persistence_paths(label);
        Self
    }
}

#[cfg(test)]
impl Drop for PersistenceGuard {
    fn drop(&mut self) {
        clear_temp_persistence_paths();
    }
}

#[cfg(test)]
mod sessions_key_tests {
    use super::*;
    use crate::protocol::{SessionGroup, SessionSummary};

    fn make_group(cwd: Option<&str>, ids: &[&str]) -> SessionGroup {
        SessionGroup {
            cwd: cwd.map(String::from),
            latest_activity: None,
            sessions: ids
                .iter()
                .map(|id| SessionSummary {
                    session_id: id.to_string(),
                    title: None,
                    cwd: None,
                    created_at: None,
                    updated_at: None,
                    parent_session_id: None,
                    has_children: false,
                })
                .collect(),
        }
    }

    // ── Down / Up navigation ──────────────────────────────────────────────────

    #[test]
    fn down_moves_cursor_forward() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2"])];
        // items: [GroupHeader, Session(s1), Session(s2)]
        assert_eq!(app.session_cursor, 0);
        apply_sessions_key(&mut app, KeyCode::Down);
        assert_eq!(app.session_cursor, 1);
        apply_sessions_key(&mut app, KeyCode::Down);
        assert_eq!(app.session_cursor, 2);
    }

    #[test]
    fn down_from_last_item_reaches_button_slot() {
        let mut app = App::new();
        // items: [GroupHeader(0), Session(1)] → button slot = 2
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        app.session_cursor = 1; // last item (Session s1)
        apply_sessions_key(&mut app, KeyCode::Down);
        assert_eq!(app.session_cursor, 2); // moved to button slot
    }

    #[test]
    fn up_moves_cursor_back() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2"])];
        app.session_cursor = 2;
        apply_sessions_key(&mut app, KeyCode::Up);
        assert_eq!(app.session_cursor, 1);
    }

    #[test]
    fn up_does_not_go_below_zero() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        app.session_cursor = 0;
        apply_sessions_key(&mut app, KeyCode::Up);
        assert_eq!(app.session_cursor, 0);
    }

    // ── Enter on GroupHeader toggles collapse ─────────────────────────────────

    #[test]
    fn enter_on_header_collapses_group() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        app.session_cursor = 0; // on the header
        let action = apply_sessions_key(&mut app, KeyCode::Enter);
        assert_eq!(action, SessionKeyAction::None);
        assert!(app.collapsed_groups.contains("/a"));
    }

    #[test]
    fn enter_on_collapsed_header_expands_group() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        app.collapsed_groups.insert("/a".to_string());
        app.session_cursor = 0; // on the header
        let action = apply_sessions_key(&mut app, KeyCode::Enter);
        assert_eq!(action, SessionKeyAction::None);
        assert!(!app.collapsed_groups.contains("/a"));
    }

    // ── Enter on Session loads it ─────────────────────────────────────────────

    #[test]
    fn enter_on_session_returns_load_action() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["abc12345"])];
        app.session_cursor = 1; // Session row
        let action = apply_sessions_key(&mut app, KeyCode::Enter);
        assert_eq!(
            action,
            SessionKeyAction::LoadSession {
                session_id: "abc12345".to_string()
            }
        );
    }

    // ── Delete on Session removes it ─────────────────────────────────────────

    #[test]
    fn delete_on_session_returns_delete_action_and_removes() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2"])];
        app.session_cursor = 1; // Session s1
        let action = apply_sessions_key(&mut app, KeyCode::Delete);
        assert_eq!(
            action,
            SessionKeyAction::DeleteSession {
                session_id: "s1".to_string()
            }
        );
        // s1 removed; group still has s2
        assert_eq!(app.session_groups[0].sessions.len(), 1);
        assert_eq!(app.session_groups[0].sessions[0].session_id, "s2");
    }

    #[test]
    fn delete_removes_empty_group() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["only"])];
        app.session_cursor = 1;
        apply_sessions_key(&mut app, KeyCode::Delete);
        // Group removed entirely
        assert!(app.session_groups.is_empty());
    }

    #[test]
    fn delete_on_header_is_noop() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        app.session_cursor = 0; // GroupHeader
        let action = apply_sessions_key(&mut app, KeyCode::Delete);
        assert_eq!(action, SessionKeyAction::None);
        // Session still there
        assert_eq!(app.session_groups[0].sessions.len(), 1);
    }

    // ── Char appends to filter and resets cursor ──────────────────────────────

    #[test]
    fn char_appends_to_filter_and_resets_cursor() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        app.session_cursor = 1;
        apply_sessions_key(&mut app, KeyCode::Char('x'));
        assert_eq!(app.session_filter, "x");
        assert_eq!(app.session_cursor, 0);
    }

    #[test]
    fn backspace_removes_last_filter_char_and_resets_cursor() {
        let mut app = App::new();
        app.session_filter = "ab".to_string();
        app.session_cursor = 2;
        apply_sessions_key(&mut app, KeyCode::Backspace);
        assert_eq!(app.session_filter, "a");
        assert_eq!(app.session_cursor, 0);
    }

    #[test]
    fn backspace_on_empty_filter_is_noop() {
        let mut app = App::new();
        apply_sessions_key(&mut app, KeyCode::Backspace);
        assert_eq!(app.session_filter, "");
        assert_eq!(app.session_cursor, 0);
    }

    // ── Collapse clamps cursor ────────────────────────────────────────────────

    #[test]
    fn collapse_clamps_cursor_when_selected_row_disappears() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2"])];
        app.session_cursor = 2; // pointing at Session s2
        // Collapsing /a while cursor is on s2 should clamp to the header (idx 0)
        apply_sessions_key(&mut app, KeyCode::Enter); // cursor=2 → on s2, wait...
        // Actually cursor=2 is Session s2; Enter sends LoadSession not collapse.
        // We need to test collapse-clamping by setting cursor on header first,
        // then collapse, then verify the previously-selected session index gets clamped.
        // Reset: cursor on header, collapse, cursor stays at 0.
        let mut app2 = App::new();
        app2.session_groups = vec![make_group(Some("/a"), &["s1", "s2"])];
        app2.session_cursor = 0; // header
        apply_sessions_key(&mut app2, KeyCode::Enter); // collapse
        // 1 item visible (just header). cursor must be <= 0.
        assert_eq!(app2.session_cursor, 0);
        assert!(app2.collapsed_groups.contains("/a"));
    }

    // ── ShowMore Enter opens session popup ────────────────────────────────────

    #[test]
    fn enter_on_show_more_opens_session_popup() {
        let mut app = App::new();
        // 4 sessions → header(0) + s1(1) + s2(2) + s3(3) + ShowMore(4)
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2", "s3", "s4"])];
        app.session_cursor = 4; // ShowMore row
        let action = apply_sessions_key(&mut app, KeyCode::Enter);
        assert_eq!(action, SessionKeyAction::None);
        assert_eq!(app.popup, crate::app::Popup::SessionSelect);
        assert_eq!(app.session_cursor, 0);
        assert!(app.session_filter.is_empty());
    }

    // ── New Session button slot ───────────────────────────────────────────────

    #[test]
    fn down_can_reach_button_slot() {
        let mut app = App::new();
        // 1 group with 1 session → items: [GroupHeader(0), Session(1)]
        // button slot = items.len() = 2
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        app.session_cursor = 1; // on Session
        apply_sessions_key(&mut app, KeyCode::Down);
        assert_eq!(app.session_cursor, 2); // on button slot
    }

    #[test]
    fn down_does_not_exceed_button_slot() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        app.session_cursor = 2; // already on button slot
        apply_sessions_key(&mut app, KeyCode::Down);
        assert_eq!(app.session_cursor, 2); // stays
    }

    #[test]
    fn down_reaches_button_when_no_sessions() {
        let mut app = App::new();
        // No sessions → items is empty, button slot = 0
        app.session_cursor = 0;
        apply_sessions_key(&mut app, KeyCode::Down);
        // items.len() == 0, button is slot 0, can't go further
        assert_eq!(app.session_cursor, 0);
    }

    #[test]
    fn enter_on_button_slot_returns_new_session() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        // items: [GroupHeader(0), Session(1)] → button slot = 2
        app.session_cursor = 2;
        let action = apply_sessions_key(&mut app, KeyCode::Enter);
        assert_eq!(action, SessionKeyAction::NewSession);
    }

    #[test]
    fn enter_on_button_slot_no_sessions_returns_new_session() {
        let mut app = App::new();
        // No items → button slot = 0
        app.session_cursor = 0;
        let action = apply_sessions_key(&mut app, KeyCode::Enter);
        assert_eq!(action, SessionKeyAction::NewSession);
    }

    #[test]
    fn delete_on_button_slot_is_noop() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        app.session_cursor = 2; // button slot
        let action = apply_sessions_key(&mut app, KeyCode::Delete);
        assert_eq!(action, SessionKeyAction::None);
        assert_eq!(app.session_groups[0].sessions.len(), 1); // unchanged
    }

    // ── q quits ───────────────────────────────────────────────────────────────
    // (q is handled in handle_sessions_key, not apply_sessions_key — tested
    //  via the existing integration path)
}

#[cfg(test)]
mod session_popup_key_tests {
    use super::*;
    use crate::protocol::{SessionGroup, SessionSummary};

    fn make_group(cwd: Option<&str>, ids: &[&str]) -> SessionGroup {
        SessionGroup {
            cwd: cwd.map(String::from),
            latest_activity: None,
            sessions: ids
                .iter()
                .map(|id| SessionSummary {
                    session_id: id.to_string(),
                    title: Some(format!("Session {id}")),
                    cwd: cwd.map(String::from),
                    created_at: None,
                    updated_at: None,
                    parent_session_id: None,
                    has_children: false,
                })
                .collect(),
        }
    }

    // ── Down / Up navigation ──────────────────────────────────────────────────

    #[test]
    fn popup_down_moves_cursor_forward() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2"])];
        // visible: [GroupHeader, Session(s1), Session(s2)]
        assert_eq!(app.session_cursor, 0);
        apply_popup_session_key(&mut app, KeyCode::Down);
        assert_eq!(app.session_cursor, 1);
        apply_popup_session_key(&mut app, KeyCode::Down);
        assert_eq!(app.session_cursor, 2);
    }

    #[test]
    fn popup_down_clamps_at_last_item() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        // visible: [GroupHeader(0), Session(1)] — max idx = 1
        app.session_cursor = 1;
        apply_popup_session_key(&mut app, KeyCode::Down);
        assert_eq!(app.session_cursor, 1); // clamped, no button slot
    }

    #[test]
    fn popup_up_moves_cursor_back() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2"])];
        app.session_cursor = 2;
        apply_popup_session_key(&mut app, KeyCode::Up);
        assert_eq!(app.session_cursor, 1);
    }

    #[test]
    fn popup_up_does_not_go_below_zero() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        app.session_cursor = 0;
        apply_popup_session_key(&mut app, KeyCode::Up);
        assert_eq!(app.session_cursor, 0);
    }

    // ── Enter on GroupHeader toggles popup collapse ───────────────────────────

    #[test]
    fn popup_enter_on_header_collapses_group() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        app.session_cursor = 0; // GroupHeader
        let action = apply_popup_session_key(&mut app, KeyCode::Enter);
        assert_eq!(action, SessionKeyAction::None);
        assert!(app.popup_collapsed_groups.contains("/a"));
        // start-page state untouched
        assert!(!app.collapsed_groups.contains("/a"));
    }

    #[test]
    fn popup_enter_on_collapsed_header_expands_group() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        app.popup_collapsed_groups.insert("/a".to_string());
        app.session_cursor = 0;
        let action = apply_popup_session_key(&mut app, KeyCode::Enter);
        assert_eq!(action, SessionKeyAction::None);
        assert!(!app.popup_collapsed_groups.contains("/a"));
    }

    #[test]
    fn popup_collapse_clamps_cursor() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        // [GroupHeader(0), Session(s1, 1), Session(s2, 2)]
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2"])];
        app.session_cursor = 0; // header
        // Collapse /a → only header remains
        apply_popup_session_key(&mut app, KeyCode::Enter);
        // Cursor must be 0 (clamped to header)
        assert_eq!(app.session_cursor, 0);
        assert!(app.popup_collapsed_groups.contains("/a"));
    }

    // ── Enter on Session loads it ─────────────────────────────────────────────

    #[test]
    fn popup_enter_on_session_returns_load_and_closes_popup() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_groups = vec![make_group(Some("/a"), &["abc12345"])];
        app.session_cursor = 1; // Session row
        let action = apply_popup_session_key(&mut app, KeyCode::Enter);
        assert_eq!(
            action,
            SessionKeyAction::LoadSession {
                session_id: "abc12345".to_string()
            }
        );
        assert_eq!(app.popup, Popup::None);
    }

    // ── Enter with all groups shows all sessions (no cap) ─────────────────────

    #[test]
    fn popup_enter_can_reach_session_beyond_start_page_cap() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        // 5 sessions — start page would cap at 3; popup shows all
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2", "s3", "s4", "s5"])];
        // visible: [Header(0), s1(1), s2(2), s3(3), s4(4), s5(5)]
        app.session_cursor = 5;
        let action = apply_popup_session_key(&mut app, KeyCode::Enter);
        assert_eq!(
            action,
            SessionKeyAction::LoadSession {
                session_id: "s5".to_string()
            }
        );
    }

    // ── Delete on Session removes it ─────────────────────────────────────────

    #[test]
    fn popup_delete_on_session_returns_delete_and_removes() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2"])];
        app.session_cursor = 1; // Session s1
        let action = apply_popup_session_key(&mut app, KeyCode::Delete);
        assert_eq!(
            action,
            SessionKeyAction::DeleteSession {
                session_id: "s1".to_string()
            }
        );
        assert_eq!(app.session_groups[0].sessions.len(), 1);
        assert_eq!(app.session_groups[0].sessions[0].session_id, "s2");
    }

    #[test]
    fn popup_delete_removes_empty_group() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_groups = vec![make_group(Some("/a"), &["only"])];
        app.session_cursor = 1;
        apply_popup_session_key(&mut app, KeyCode::Delete);
        assert!(app.session_groups.is_empty());
    }

    #[test]
    fn popup_delete_on_header_is_noop() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        app.session_cursor = 0; // GroupHeader
        let action = apply_popup_session_key(&mut app, KeyCode::Delete);
        assert_eq!(action, SessionKeyAction::None);
        assert_eq!(app.session_groups[0].sessions.len(), 1);
    }

    // ── Esc closes popup ──────────────────────────────────────────────────────

    #[test]
    fn popup_esc_closes_popup() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        apply_popup_session_key(&mut app, KeyCode::Esc);
        assert_eq!(app.popup, Popup::None);
    }

    // ── Filter: Char appends, Backspace removes, both reset cursor ────────────

    #[test]
    fn popup_char_appends_to_filter_and_resets_cursor() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        app.session_cursor = 1;
        apply_popup_session_key(&mut app, KeyCode::Char('x'));
        assert_eq!(app.session_filter, "x");
        assert_eq!(app.session_cursor, 0);
    }

    #[test]
    fn popup_ctrl_n_opens_new_session_popup() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.conn = crate::app::ConnState::Connected;
        app.launch_cwd = Some("/launch".into());

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        handle_session_popup_key(
            &mut app,
            KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
                kind: crossterm::event::KeyEventKind::Press,
                state: crossterm::event::KeyEventState::NONE,
            },
            &cmd_tx,
        )
        .unwrap();

        assert_eq!(app.popup, Popup::NewSession);
        assert_eq!(app.new_session_path, "/launch");
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn popup_plain_n_still_filters_instead_of_creating_session() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        handle_session_popup_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &cmd_tx,
        )
        .unwrap();

        assert_eq!(app.popup, Popup::SessionSelect);
        assert_eq!(app.session_filter, "n");
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn global_ctrl_x_n_opens_new_session_popup() {
        let mut app = App::new();
        app.conn = crate::app::ConnState::Connected;
        app.launch_cwd = Some("/launch".into());
        let (tx, mut rx) = mpsc::unbounded_channel();

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &tx,
        )
        .unwrap();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &tx,
        )
        .unwrap();

        assert_eq!(app.popup, Popup::NewSession);
        assert_eq!(app.new_session_path, "/launch");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn new_session_popup_enter_with_empty_path_uses_launch_cwd() {
        let mut app = App::new();
        app.conn = crate::app::ConnState::Connected;
        app.popup = Popup::NewSession;
        app.launch_cwd = Some("/launch".into());
        app.new_session_path.clear();
        app.new_session_cursor = 0;

        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_new_session_popup_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &tx,
        )
        .unwrap();

        assert_eq!(app.popup, Popup::None);
        assert!(matches!(
            rx.try_recv(),
            Ok(ClientMsg::NewSession {
                cwd: Some(ref cwd),
                request_id: None
            }) if cwd == "/launch"
        ));
    }

    #[test]
    fn new_session_popup_enter_normalizes_relative_path_to_absolute() {
        let mut app = App::new();
        app.conn = crate::app::ConnState::Connected;
        app.popup = Popup::NewSession;
        app.launch_cwd = Some("/launch".into());
        app.new_session_path = "proj/subdir".into();
        app.new_session_cursor = app.new_session_path.len();

        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_new_session_popup_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &tx,
        )
        .unwrap();

        assert!(matches!(
            rx.try_recv(),
            Ok(ClientMsg::NewSession {
                cwd: Some(ref cwd),
                request_id: None
            }) if cwd == "/launch/proj/subdir"
        ));
    }

    #[test]
    fn new_session_popup_tab_accepts_selected_completion() {
        let mut app = App::new();
        app.popup = Popup::NewSession;
        app.new_session_completion = Some(crate::app::PathCompletionState {
            query: "pro".into(),
            selected_index: 0,
            results: vec![crate::app::FileIndexEntryLite {
                path: "/launch/project".into(),
                is_dir: true,
            }],
        });

        let (tx, _rx) = mpsc::unbounded_channel();
        handle_new_session_popup_key(
            &mut app,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &tx,
        )
        .unwrap();

        assert_eq!(app.new_session_path, "/launch/project/");
        assert!(app.new_session_completion.is_none());
    }

    #[test]
    fn handle_key_routes_tab_to_new_session_popup_before_global_mode_switch() {
        let mut app = App::new();
        app.conn = crate::app::ConnState::Connected;
        app.popup = Popup::NewSession;
        app.agent_mode = "build".into();
        app.new_session_completion = Some(crate::app::PathCompletionState {
            query: "pro".into(),
            selected_index: 0,
            results: vec![crate::app::FileIndexEntryLite {
                path: "/launch/project".into(),
                is_dir: true,
            }],
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &tx,
        )
        .unwrap();

        assert_eq!(app.new_session_path, "/launch/project/");
        assert!(app.new_session_completion.is_none());
        assert_eq!(app.agent_mode, "build");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn popup_backspace_removes_last_filter_char_and_resets_cursor() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_filter = "ab".to_string();
        app.session_cursor = 2;
        apply_popup_session_key(&mut app, KeyCode::Backspace);
        assert_eq!(app.session_filter, "a");
        assert_eq!(app.session_cursor, 0);
    }

    // ── multiple groups: navigation crosses group boundaries ─────────────────

    #[test]
    fn popup_down_crosses_group_boundary() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_groups = vec![
            make_group(Some("/a"), &["s1"]),
            make_group(Some("/b"), &["s2"]),
        ];
        // visible: [Header /a (0), Session s1 (1), Header /b (2), Session s2 (3)]
        app.session_cursor = 1; // s1
        apply_popup_session_key(&mut app, KeyCode::Down);
        assert_eq!(app.session_cursor, 2); // Header /b
        apply_popup_session_key(&mut app, KeyCode::Down);
        assert_eq!(app.session_cursor, 3); // s2
    }

    // ── collapse in popup does not affect start-page navigation ──────────────

    #[test]
    fn popup_collapse_independent_of_start_page() {
        let mut app = App::new();
        app.popup = Popup::SessionSelect;
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        // collapse in popup
        app.session_cursor = 0;
        apply_popup_session_key(&mut app, KeyCode::Enter);
        assert!(app.popup_collapsed_groups.contains("/a"));
        // start page state untouched
        assert!(!app.collapsed_groups.contains("/a"));
    }
}

#[cfg(test)]
mod chord_reasoning_effort_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serial_test::serial;
    use tokio::sync::mpsc;

    fn ctrl_t() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)
    }

    // ── Ctrl+t cycles reasoning effort and sends message ─────────────────────

    #[test]
    #[serial]
    fn ctrl_t_cycles_effort_and_sends_msg() {
        install_temp_persistence_paths("ctrl-t-1");
        let _guard = PersistenceGuard::new("main-test");
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        assert_eq!(app.reasoning_effort, None);

        handle_key(&mut app, ctrl_t(), &tx).unwrap();

        assert_eq!(app.reasoning_effort, Some("low".into()));
        let msg = rx.try_recv().expect("expected SetReasoningEffort message");
        match msg {
            ClientMsg::SetReasoningEffort { reasoning_effort } => {
                assert_eq!(reasoning_effort, "low");
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn ctrl_t_full_cycle_sends_auto_on_wrap() {
        let _guard = PersistenceGuard::new("main-test");
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.reasoning_effort = Some("max".into());

        handle_key(&mut app, ctrl_t(), &tx).unwrap();

        assert_eq!(app.reasoning_effort, None);
        let msg = rx.try_recv().expect("expected SetReasoningEffort message");
        match msg {
            ClientMsg::SetReasoningEffort { reasoning_effort } => {
                assert_eq!(reasoning_effort, "auto");
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn ctrl_t_status_updated() {
        let _guard = PersistenceGuard::new("main-test");
        let (tx, _rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        handle_key(&mut app, ctrl_t(), &tx).unwrap();
        // status should reflect the new level
        assert!(
            app.status.contains("low"),
            "expected status to mention 'low', got: {}",
            app.status
        );
    }
}

#[cfg(test)]
mod reasoning_effort_integration_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serial_test::serial;
    use tokio::sync::mpsc;

    fn make_model(provider: &str, model: &str) -> crate::protocol::ModelEntry {
        crate::protocol::ModelEntry {
            id: format!("{provider}/{model}"),
            label: model.into(),
            provider: provider.into(),
            model: model.into(),
            node_id: None,
            family: None,
            quant: None,
        }
    }

    fn chord_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn tab_key() -> KeyEvent {
        KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
    }

    // ── Ctrl+x t caches mode state per session ──────────────────────────────

    #[test]
    #[serial]
    fn ctrl_t_caches_mode_state_for_session() {
        let _guard = PersistenceGuard::new("main-test");
        let (tx, _rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.session_id = Some("s1".into());
        app.agent_mode = "build".into();
        app.current_provider = Some("anthropic".into());
        app.current_model = Some("claude-sonnet".into());
        app.conn = app::ConnState::Connected;

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &tx,
        )
        .unwrap();

        let cms = &app.session_cache["s1"]["build"];
        assert_eq!(cms.model, "anthropic/claude-sonnet");
        assert_eq!(cms.effort, Some("low".into()));
    }

    // ── Tab: saves outgoing, restores incoming ────────────────────────────────

    #[test]
    #[serial]
    fn tab_saves_outgoing_and_restores_incoming_mode_state() {
        let _guard = PersistenceGuard::new("main-test");
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.conn = app::ConnState::Connected;
        app.session_id = Some("s1".into());
        app.agent_mode = "build".into();
        app.current_provider = Some("anthropic".into());
        app.current_model = Some("claude-sonnet".into());
        app.reasoning_effort = Some("high".into());
        app.models = vec![
            make_model("anthropic", "claude-sonnet"),
            make_model("openai", "gpt-4o"),
        ];

        // Pre-cache plan mode state for this session
        app.session_cache.entry("s1".into()).or_default().insert(
            "plan".into(),
            app::CachedModeState {
                model: "openai/gpt-4o".into(),
                effort: Some("low".into()),
            },
        );

        // Tab → switch build → plan
        handle_key(&mut app, tab_key(), &tx).unwrap();

        // Outgoing build state saved
        let build = &app.session_cache["s1"]["build"];
        assert_eq!(build.model, "anthropic/claude-sonnet");
        assert_eq!(build.effort, Some("high".into()));

        // Incoming plan state restored
        assert_eq!(app.agent_mode, "plan");
        assert_eq!(app.current_provider.as_deref(), Some("openai"));
        assert_eq!(app.current_model.as_deref(), Some("gpt-4o"));
        assert_eq!(app.reasoning_effort, Some("low".into()));

        let msgs: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            msgs.iter()
                .any(|m| matches!(m, ClientMsg::SetSessionModel { .. })),
            "expected SetSessionModel: {msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                ClientMsg::SetReasoningEffort { reasoning_effort }
                if reasoning_effort == "low"
            )),
            "expected SetReasoningEffort(low): {msgs:?}"
        );
    }

    #[test]
    #[serial]
    fn tab_no_cache_entry_leaves_model_and_effort_unchanged() {
        let _guard = PersistenceGuard::new("main-test");
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.conn = app::ConnState::Connected;
        app.session_id = Some("s1".into());
        app.agent_mode = "build".into();
        app.current_provider = Some("anthropic".into());
        app.current_model = Some("claude-sonnet".into());
        app.reasoning_effort = Some("high".into());
        // No plan cache entry

        handle_key(&mut app, tab_key(), &tx).unwrap();

        // Mode switched but model/effort unchanged (no cache to restore from)
        assert_eq!(app.agent_mode, "plan");
        assert_eq!(app.reasoning_effort, Some("high".into()));
        assert_eq!(app.current_model.as_deref(), Some("claude-sonnet"));
        let msgs: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ClientMsg::SetReasoningEffort { .. })),
            "no SetReasoningEffort expected: {msgs:?}"
        );
    }

    // ── Model select: drops effort to auto ────────────────────────────────────

    #[test]
    #[serial]
    fn model_select_drops_effort_to_auto() {
        let _guard = PersistenceGuard::new("main-test");
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.conn = app::ConnState::Connected;
        app.session_id = Some("s1".into());
        app.popup = app::Popup::ModelSelect;
        app.agent_mode = "build".into();
        app.current_provider = Some("anthropic".into());
        app.current_model = Some("claude-sonnet".into());
        app.reasoning_effort = Some("high".into());
        app.models = vec![make_model("anthropic", "claude-opus")];
        app.model_cursor = 0;

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &tx,
        )
        .unwrap();

        // Effort dropped to auto
        assert_eq!(app.reasoning_effort, None);

        let msgs: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                ClientMsg::SetReasoningEffort { reasoning_effort }
                if reasoning_effort == "auto"
            )),
            "expected SetReasoningEffort(auto): {msgs:?}"
        );
    }

    #[test]
    #[serial]
    fn model_select_caches_new_model_with_auto_effort() {
        let _guard = PersistenceGuard::new("main-test");
        let (tx, _rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.conn = app::ConnState::Connected;
        app.session_id = Some("s1".into());
        app.popup = app::Popup::ModelSelect;
        app.agent_mode = "build".into();
        app.current_provider = Some("anthropic".into());
        app.current_model = Some("claude-sonnet".into());
        app.reasoning_effort = Some("high".into());
        app.models = vec![make_model("anthropic", "claude-opus")];
        app.model_cursor = 0;

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &tx,
        )
        .unwrap();

        let cms = &app.session_cache["s1"]["build"];
        assert_eq!(cms.model, "anthropic/claude-opus");
        assert_eq!(cms.effort, None); // auto
    }

    #[test]
    #[serial]
    fn model_select_no_effort_msg_when_already_auto() {
        let _guard = PersistenceGuard::new("main-test");
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.conn = app::ConnState::Connected;
        app.session_id = Some("s1".into());
        app.popup = app::Popup::ModelSelect;
        app.agent_mode = "build".into();
        app.current_provider = Some("anthropic".into());
        app.current_model = Some("claude-sonnet".into());
        app.reasoning_effort = None; // already auto
        app.models = vec![make_model("anthropic", "claude-opus")];
        app.model_cursor = 0;

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &tx,
        )
        .unwrap();

        let msgs: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ClientMsg::SetReasoningEffort { .. })),
            "no SetReasoningEffort when already auto: {msgs:?}"
        );
    }

    // ── reasoning_effort server push caches per session+mode ──────────────────

    #[test]
    fn server_push_caches_effort_for_session_and_mode() {
        let mut app = App::new();
        app.session_id = Some("s1".into());
        app.agent_mode = "build".into();
        app.current_provider = Some("anthropic".into());
        app.current_model = Some("claude-sonnet".into());

        app.handle_server_msg(crate::protocol::RawServerMsg {
            msg_type: "reasoning_effort".into(),
            data: Some(serde_json::json!({ "reasoning_effort": "medium" })),
        });

        let cms = &app.session_cache["s1"]["build"];
        assert_eq!(cms.model, "anthropic/claude-sonnet");
        assert_eq!(cms.effort, Some("medium".into()));
    }

    #[test]
    fn server_push_auto_caches_none_effort() {
        let mut app = App::new();
        app.session_id = Some("s1".into());
        app.agent_mode = "build".into();
        app.current_provider = Some("anthropic".into());
        app.current_model = Some("claude-sonnet".into());
        app.reasoning_effort = Some("high".into());

        app.handle_server_msg(crate::protocol::RawServerMsg {
            msg_type: "reasoning_effort".into(),
            data: Some(serde_json::json!({ "reasoning_effort": "auto" })),
        });

        let cms = &app.session_cache["s1"]["build"];
        assert_eq!(cms.effort, None);
    }
}
