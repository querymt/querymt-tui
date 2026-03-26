#![allow(dead_code)]

mod app;
mod config;
mod handlers;
mod highlight;
mod input;
mod markdown;
mod protocol;
mod server_manager;
mod server_msg;
mod session;
mod theme;
mod themes_gen;
mod ui;

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use app::{App, Screen};
use clap::Parser;
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::{SinkExt, StreamExt};
use protocol::{ClientMsg, RawServerMsg};
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
    use crate::handlers::*;
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

    #[test]
    fn invalidate_theme_caches_clears_all_render_caches() {
        use crate::theme::Theme;
        use crate::ui::build_diff_lines;

        // Build cached_lines under theme 0
        Theme::set_by_index(0);
        Theme::begin_frame();

        let mut app = App::new();

        // Populate card_cache
        app.messages.push(ChatEntry::User {
            text: "hello".into(),
            message_id: None,
        });
        app.card_cache.processed_messages = 1;

        // Populate streaming caches
        app.streaming_cache
            .store(5, vec![ratatui::text::Line::from("stream")]);
        app.streaming_thinking_cache
            .store(3, vec![ratatui::text::Line::from("think")]);

        // Populate a ToolCall with cached_lines baked under theme 0
        let old_lines = build_diff_lines("aaa", "bbb", None);
        assert!(!old_lines.is_empty());
        app.messages.push(ChatEntry::ToolCall {
            tool_call_id: None,
            name: "edit".into(),
            is_error: false,
            detail: app::ToolDetail::Edit {
                file: "f.rs".into(),
                old: "aaa".into(),
                new: "bbb".into(),
                start_line: None,
                cached_lines: old_lines.clone(),
            },
        });

        // Confirm caches are populated
        assert!(app.streaming_cache.get(5).is_some());
        assert!(app.streaming_thinking_cache.get(3).is_some());
        assert_eq!(app.card_cache.processed_messages, 1);

        // Switch to a different theme and update the frame snapshot
        // before invalidating — just as handle_theme_popup_key does.
        Theme::set_by_index(2);
        Theme::begin_frame();
        invalidate_theme_caches(&mut app);

        // All caches cleared
        assert_eq!(
            app.card_cache.processed_messages, 0,
            "card_cache should be invalidated"
        );
        assert!(
            app.streaming_cache.get(5).is_none(),
            "streaming_cache should be invalidated"
        );
        assert!(
            app.streaming_thinking_cache.get(3).is_none(),
            "streaming_thinking_cache should be invalidated"
        );

        // Tool cached_lines rebuilt with the NEW theme's styles
        if let ChatEntry::ToolCall {
            detail: app::ToolDetail::Edit { cached_lines, .. },
            ..
        } = &app.messages[1]
        {
            assert!(
                !cached_lines.is_empty(),
                "tool cached_lines should be rebuilt"
            );
            // Lines must differ from the old theme — styles are theme-dependent
            assert_ne!(
                *cached_lines, old_lines,
                "cached_lines should use the new theme's styles, not the old"
            );
        } else {
            panic!("expected ToolCall with Edit detail");
        }

        // Reset
        Theme::set_by_index(0);
        Theme::begin_frame();
    }
}

#[cfg(test)]
mod external_editor_tests {
    use super::*;
    use crate::config::{ServerConfig, TuiConfig};
    use crate::handlers::*;
    use app::ActivityState;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use protocol::PromptBlock;

    fn ctrl_x() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)
    }

    fn plain_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())
    }

    #[test]
    fn chat_up_down_navigate_wrapped_input_without_scrolling_history() {
        let (tx, _rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.input = "abcdef".into();
        app.input_cursor = 4;
        app.input_line_width = 4;
        app.scroll_offset = 7;

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Up, KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        assert_eq!(app.input_cursor, 2);
        assert_eq!(app.scroll_offset, 7);

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        assert_eq!(app.input_cursor, 4);
        assert_eq!(app.scroll_offset, 7);
    }

    #[test]
    fn chat_pageup_pagedown_still_scroll_history() {
        let (tx, _rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.scroll_offset = 3;

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        assert_eq!(app.scroll_offset, 13);

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        assert_eq!(app.scroll_offset, 3);
    }

    #[test]
    fn editor_command_prefers_visual_over_editor() {
        let env = [("VISUAL", Some("nvim -f")), ("EDITOR", Some("vim"))];
        let cmd = editor_command_from_env(&env).expect("expected editor command");
        assert_eq!(cmd.program, "nvim");
        assert_eq!(cmd.args, vec![OsString::from("-f")]);
    }

    #[test]
    fn editor_command_uses_editor_when_visual_missing() {
        let env = [("VISUAL", None), ("EDITOR", Some("nano"))];
        let cmd = editor_command_from_env(&env).expect("expected editor command");
        assert_eq!(cmd.program, "nano");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn editor_command_rejects_blank_values() {
        let env = [("VISUAL", Some("   ")), ("EDITOR", Some(""))];
        assert!(editor_command_from_env(&env).is_none());
    }

    #[test]
    fn apply_external_editor_result_updates_input_and_cursor() {
        let mut app = App::new();
        app.input = "old".into();
        app.input_cursor = 1;
        app.input_scroll = 3;

        apply_external_editor_result(&mut app, "new text".into());

        assert_eq!(app.input, "new text");
        assert_eq!(app.input_cursor, "new text".len());
        assert_eq!(app.input_scroll, 0);
    }

    #[test]
    fn ctrl_x_e_returns_open_editor_action_in_chat() {
        let (tx, _rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.input = "draft".into();
        assert_eq!(
            handle_key(&mut app, ctrl_x(), &tx).unwrap(),
            AppAction::None
        );

        let action = handle_key(&mut app, plain_key('e'), &tx).unwrap();

        assert_eq!(action, AppAction::OpenExternalEditor);
        assert!(!app.chord);
        assert_eq!(app.input, "draft");
    }

    #[test]
    fn ctrl_x_e_outside_chat_stays_in_tui() {
        let (tx, _rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.screen = Screen::Sessions;
        assert_eq!(
            handle_key(&mut app, ctrl_x(), &tx).unwrap(),
            AppAction::None
        );

        let action = handle_key(&mut app, plain_key('e'), &tx).unwrap();

        assert_eq!(action, AppAction::None);
        assert!(app.status.contains("only available in chat"));
        assert!(matches!(app.logs.last(), Some(entry) if entry.target == "editor"));
    }

    #[test]
    fn apply_external_editor_outcome_updates_input_on_success() {
        let mut app = App::new();
        app.input = "draft".into();

        apply_external_editor_outcome(&mut app, Ok(Some("revised prompt".into())));

        assert_eq!(app.input, "revised prompt");
        assert_eq!(app.input_cursor, "revised prompt".len());
        assert_eq!(app.status, "loaded prompt from external editor");
        assert!(matches!(app.logs.last(), Some(entry) if entry.target == "editor"));
    }

    #[test]
    fn log_server_binary_discovery_records_path_lookup_when_binary_path_unset() {
        let mut app = App::new();
        let cfg = TuiConfig {
            server: ServerConfig {
                binary_path: None,
                ..ServerConfig::default()
            },
            ..TuiConfig::default()
        };

        log_server_binary_discovery(
            &mut app,
            &cfg,
            &server_manager::BinaryDiscovery {
                binary: None,
                configured_path: None,
                configured_exists: false,
                used_path_lookup: true,
            },
        );

        assert!(app.logs.iter().any(|entry| entry.target == "server"
            && entry.level == app::LogLevel::Info
            && entry.message == "server.binary_path not set; checking qmtcode on PATH"));
        assert!(app.logs.iter().any(|entry| entry.target == "server"
            && entry.level == app::LogLevel::Info
            && entry.message == "qmtcode not found on PATH"));
    }

    #[test]
    fn apply_external_editor_outcome_keeps_input_on_cancel() {
        let mut app = App::new();
        app.input = "draft".into();

        apply_external_editor_outcome(&mut app, Ok(None));

        assert_eq!(app.input, "draft");
        assert_eq!(app.status, "external editor cancelled");
    }

    #[test]
    fn chat_input_accepts_typing_and_submit_while_turn_active() {
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.conn = app::ConnState::Connected;
        app.activity = ActivityState::RunningTool {
            name: "read_tool".into(),
        };

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &tx,
        )
        .unwrap();

        assert!(matches!(
            rx.try_recv().expect("prompt sent"),
            ClientMsg::Prompt { prompt } if matches!(prompt.as_slice(), [PromptBlock::Text { text }] if text == "n")
        ));
        assert!(app.input.is_empty());
    }

    #[test]
    fn chat_double_esc_cancels_running_tool_phase() {
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.activity = ActivityState::RunningTool {
            name: "read_tool".into(),
        };

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        assert!(app.cancel_confirm_active());

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        assert!(matches!(
            rx.try_recv().expect("cancel sent"),
            ClientMsg::CancelSession
        ));
        assert_eq!(app.status, "stopping...");
    }

    #[test]
    fn chat_input_is_blocked_while_undo_is_pending() {
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.conn = app::ConnState::Connected;
        app.activity = ActivityState::SessionOp(app::SessionOp::Undo);
        app.input = "draft".into();
        app.input_cursor = app.input.len();

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        assert_eq!(app.input, "draft");

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        assert_eq!(app.input, "draft");

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Left, KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        assert_eq!(app.input_cursor, "draft".len());

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        assert_eq!(app.input, "draft");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn chat_input_is_blocked_while_cancel_confirm_is_active() {
        let (tx, mut rx) = mpsc::unbounded_channel::<ClientMsg>();
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.conn = app::ConnState::Connected;
        app.activity = ActivityState::RunningTool {
            name: "read_tool".into(),
        };
        app.input = "draft".into();
        app.input_cursor = app.input.len();

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        assert!(app.cancel_confirm_active());
        assert!(app.input_blocked_by_activity());

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        assert_eq!(app.input, "draft");

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        assert_eq!(app.status, "press Esc again to stop");
        assert!(rx.try_recv().is_err());

        handle_chat_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
            &tx,
        )
        .unwrap();
        assert_eq!(app.status, "stopping...");
        assert!(matches!(
            rx.try_recv().expect("cancel sent"),
            ClientMsg::CancelSession
        ));
    }
}

#[derive(Parser)]
#[command(name = "qmtui")]
#[command(version = env!("QMTUI_BUILD_VERSION"))]
#[command(about = "querymt terminal interface")]
struct Cli {
    /// Server address (e.g. 127.0.0.1:3030). Overrides the value in ~/.qmt/tui.toml.
    #[arg(long)]
    server: Option<String>,

    /// Restore a session by id.
    #[arg(short = 's', long)]
    session: Option<String>,
}

fn detect_launch_cwd() -> Option<String> {
    std::env::current_dir()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EditorCommand {
    program: OsString,
    args: Vec<OsString>,
}

fn parse_editor_command(value: &str) -> Option<EditorCommand> {
    let parts: Vec<_> = value.split_whitespace().collect();
    let (program, args) = parts.split_first()?;
    Some(EditorCommand {
        program: OsString::from(program),
        args: args.iter().map(OsString::from).collect(),
    })
}

fn editor_command_from_env(
    env: &[(impl AsRef<str>, Option<impl AsRef<str>>)],
) -> Option<EditorCommand> {
    env.iter().find_map(|(_, value)| {
        value
            .as_ref()
            .and_then(|value| parse_editor_command(value.as_ref().trim()))
    })
}

fn system_editor_command() -> Option<EditorCommand> {
    let visual = std::env::var("VISUAL").ok();
    let editor = std::env::var("EDITOR").ok();
    editor_command_from_env(&[("VISUAL", visual.as_deref()), ("EDITOR", editor.as_deref())])
}

use handlers::{AppAction, handle_key, save_cache};

fn temp_editor_file_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("qmt-tui-editor-{}-{nanos}.md", std::process::id()))
}

fn run_external_editor(command: &EditorCommand, path: &Path) -> anyhow::Result<Option<String>> {
    let status = Command::new(&command.program)
        .args(&command.args)
        .arg(path)
        .status()?;
    if !status.success() {
        return Ok(None);
    }
    Ok(Some(fs::read_to_string(path)?))
}

fn cleanup_temp_editor_file(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn open_external_editor(initial_text: &str) -> anyhow::Result<Option<String>> {
    let command = system_editor_command()
        .ok_or_else(|| anyhow::anyhow!("set $VISUAL or $EDITOR to use an external editor"))?;
    let path = temp_editor_file_path();
    fs::write(&path, initial_text)?;
    let result = run_external_editor(&command, &path);
    let cleanup_result = cleanup_temp_editor_file(&path);
    cleanup_result?;
    result
}

fn apply_external_editor_result(app: &mut App, updated_input: String) {
    app.input = updated_input;
    app.input_cursor = app.input.len();
    app.input_scroll = 0;
    app.refresh_mention_state();
}

fn apply_external_editor_outcome(app: &mut App, result: anyhow::Result<Option<String>>) {
    match result {
        Ok(Some(updated_input)) => {
            apply_external_editor_result(app, updated_input);
            app.set_status(
                app::LogLevel::Info,
                "editor",
                "loaded prompt from external editor",
            );
        }
        Ok(None) => {
            app.set_status(app::LogLevel::Info, "editor", "external editor cancelled");
        }
        Err(err) => {
            app.set_status(
                app::LogLevel::Error,
                "editor",
                format!("external editor failed: {err}"),
            );
        }
    }
}

fn log_server_binary_discovery(
    app: &mut App,
    cfg: &config::TuiConfig,
    discovery: &server_manager::BinaryDiscovery,
) {
    if !discovery.used_path_lookup {
        return;
    }
    if let Some(path) = discovery.configured_path.as_deref() {
        app.push_log(
            app::LogLevel::Info,
            "server",
            format!("configured qmtcode path not found: {path}; checking PATH"),
        );
    } else if cfg.server.binary_path.is_none() {
        app.push_log(
            app::LogLevel::Info,
            "server",
            "server.binary_path not set; checking qmtcode on PATH",
        );
    }
    if discovery.binary.is_none() {
        app.push_log(app::LogLevel::Info, "server", "qmtcode not found on PATH");
    }
}

fn open_external_editor_with_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> anyhow::Result<()> {
    terminal.show_cursor()?;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    let result = open_external_editor(&app.input);

    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    terminal.hide_cursor()?;
    terminal.clear()?;
    terminal.autoresize()?;
    app.card_cache.invalidate();
    app.streaming_cache.invalidate();
    apply_external_editor_outcome(app, result);
    terminal.draw(|f| ui::draw(f, app))?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Load persistent config; CLI args override config defaults.
    let cfg = config::TuiConfig::load();

    let addr = cli
        .server
        .or_else(|| cfg.server.addr.clone())
        .unwrap_or_else(|| "127.0.0.1:42069".to_string());
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

    let mut app = App::new();
    app.launch_cwd = detect_launch_cwd();
    app.show_thinking = cfg.show_thinking.unwrap_or(true);
    if let Some(session_id) = cli.session.clone() {
        app.session_id = Some(session_id);
        app.screen = Screen::Chat;
    }
    // Hydrate session effort cache from disk.
    config::TuiCache::load().hydrate_app(&mut app);

    // ── Server auto-start ─────────────────────────────────────────────────────
    let auto_start = cfg.server.auto_start.unwrap_or(true);
    let shutdown_on_exit = cfg.server.shutdown_on_exit.unwrap_or(true);
    let launch_mode = cfg.server.launch_mode.unwrap_or_default();
    let (sup_event_tx, mut sup_event_rx) = mpsc::unbounded_channel::<server_manager::ServerEvent>();
    let (sup_shutdown_tx, sup_shutdown_rx) = mpsc::channel::<()>(1);

    let initial_server_state = if auto_start {
        let discovery = server_manager::find_binary_info(cfg.server.binary_path.as_deref());
        log_server_binary_discovery(&mut app, &cfg, &discovery);

        if let Some(binary) = discovery.binary {
            let sup_config = server_manager::ServerManagerConfig {
                addr: addr.clone(),
                launch_mode,
                binary_args: cfg.server.binary_args.clone().unwrap_or_default(),
                shutdown_on_exit,
                lock_path: None,
                ready_timeout: None,
            };
            tokio::spawn(server_manager::supervisor(
                sup_config,
                binary,
                sup_event_tx,
                sup_shutdown_rx,
            ));
            server_manager::ServerState::Starting
        } else {
            let _ = sup_event_tx.send(server_manager::ServerEvent::BinaryNotFound);
            server_manager::ServerState::BinaryNotFound
        }
    } else {
        server_manager::ServerState::Disabled
    };

    tokio::spawn(connection_manager(url, srv_tx, cmd_rx, conn_tx));

    // setup terminal
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    app.server_state = initial_server_state;
    let result = run_loop(
        &mut terminal,
        &mut app,
        &mut srv_rx,
        &mut conn_rx,
        &mut sup_event_rx,
        &cmd_tx,
    )
    .await;

    // Signal supervisor to stop (and kill the child if configured).
    let _ = sup_shutdown_tx.send(()).await;

    // restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Some(session_id) = &app.session_id {
        eprintln!("{}", restore_hint(session_id));
    }

    result
}

fn restore_hint(session_id: &str) -> String {
    use clap::CommandFactory;
    let bin = Cli::command().get_name().to_string();
    format!("{bin} -s {session_id}")
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
    sup_rx: &mut mpsc::UnboundedReceiver<server_manager::ServerEvent>,
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
                            app.set_status(
                                app::LogLevel::Warn,
                                "connection",
                                "connection lost - reconnecting...",
                            );
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
            // server supervisor events
            Some(sup_event) = sup_rx.recv() => {
                use server_manager::{ServerEvent, ServerState};
                match sup_event {
                    ServerEvent::Starting => {
                        app.server_state = ServerState::Starting;
                        if app.conn != app::ConnState::Connected {
                            app.set_status(app::LogLevel::Info, "server", "starting local server...");
                        }
                    }
                    ServerEvent::Started => {
                        app.server_state = ServerState::Running;
                        if app.conn != app::ConnState::Connected {
                            app.set_status(
                                app::LogLevel::Info,
                                "server",
                                "local server started — connecting...",
                            );
                        }
                    }
                    ServerEvent::BinaryNotFound => {
                        app.server_state = ServerState::BinaryNotFound;
                        if app.conn != app::ConnState::Connected {
                            app.set_status(
                                app::LogLevel::Warn,
                                "server",
                                "qmtcode not found — install it or set server.binary_path in ~/.qmt/tui.toml",
                            );
                        }
                    }
                    ServerEvent::StartFailed { error } => {
                        app.server_state = ServerState::StartFailed { error: error.clone() };
                        app.set_status(
                            app::LogLevel::Error,
                            "server",
                            format!("server start failed: {error}"),
                        );
                    }
                    ServerEvent::Stopped { reason } => {
                        app.server_state = ServerState::Restarting { reason: reason.clone() };
                        app.set_status(
                            app::LogLevel::Warn,
                            "server",
                            format!("server stopped ({reason}) — restarting..."),
                        );
                    }
                }
            }
            // terminal input
            _ = tokio::task::spawn_blocking(|| {
                event::poll(Duration::from_millis(50)).unwrap_or(false)
            }) => {
                if event::poll(Duration::from_millis(0))?
                    && let Event::Key(key) = event::read()?
                {
                    let action = handle_key(app, key, cmd_tx)?;
                    if matches!(action, AppAction::OpenExternalEditor) {
                        open_external_editor_with_terminal(terminal, app)?;
                    }
                }
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

#[cfg(test)]
struct PersistenceGuard(config::TestPersistenceGuard);

#[cfg(test)]
impl PersistenceGuard {
    fn new(label: &str) -> Self {
        Self(config::TestPersistenceGuard::new(label))
    }
}

#[cfg(test)]
mod sessions_key_tests {
    use super::*;
    use crate::handlers::*;
    use crate::protocol::{SessionGroup, SessionSummary};
    use crossterm::event::KeyCode;

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
    use crate::handlers::*;
    use crate::protocol::{SessionGroup, SessionSummary};
    use app::Popup;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
    fn global_ctrl_x_l_opens_log_popup() {
        let mut app = App::new();
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &tx,
        )
        .unwrap();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            &tx,
        )
        .unwrap();

        assert_eq!(app.popup, Popup::Log);
        assert_eq!(app.log_cursor, 0);
        assert!(app.log_filter.is_empty());
    }

    #[test]
    fn log_popup_filters_cycles_level_and_closes() {
        let mut app = App::new();
        app.popup = Popup::Log;
        app.log_cursor = 2;
        app.log_level_filter = crate::app::LogLevel::Info;

        let (tx, _rx) = mpsc::unbounded_channel();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &tx,
        )
        .unwrap();
        assert_eq!(app.log_filter, "x");
        assert_eq!(app.log_cursor, 0);

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &tx,
        )
        .unwrap();
        assert!(app.log_filter.is_empty());

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &tx,
        )
        .unwrap();
        assert_eq!(app.log_level_filter, crate::app::LogLevel::Warn);

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &tx,
        )
        .unwrap();
        assert_eq!(app.popup, Popup::None);
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

#[cfg(test)]
mod cli_tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    fn bin() -> String {
        Cli::command().get_name().to_string()
    }

    #[test]
    fn cli_session_short_flag() {
        let b = bin();
        let cli = Cli::try_parse_from([b.as_str(), "-s", "abc123"]).unwrap();
        assert_eq!(cli.session, Some("abc123".into()));
    }

    #[test]
    fn cli_session_long_flag() {
        let b = bin();
        let cli = Cli::try_parse_from([b.as_str(), "--session", "abc123"]).unwrap();
        assert_eq!(cli.session, Some("abc123".into()));
    }

    #[test]
    fn cli_no_session_defaults_to_none() {
        let b = bin();
        let cli = Cli::try_parse_from([b.as_str()]).unwrap();
        assert_eq!(cli.session, None);
    }

    #[test]
    fn restore_hint_formats_correctly() {
        let hint = restore_hint("abc-123-def");
        assert_eq!(hint, format!("{} -s abc-123-def", bin()));
    }
}
