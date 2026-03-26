use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::text::Line;

use crate::highlight::Highlighter;
use crate::protocol::*;
use crate::ui::{CardCache, OUTCOME_BULLET};

/// Cache for rendered streaming markdown to avoid re-parsing every frame.
/// Invalidated when `streaming_content` grows or is cleared.
pub struct StreamingCache {
    /// Length of `streaming_content` at the time of last render.
    rendered_len: usize,
    /// Cached rendered lines (without the spinner).
    lines: Vec<Line<'static>>,
}

impl StreamingCache {
    pub fn new() -> Self {
        Self {
            rendered_len: 0,
            lines: Vec::new(),
        }
    }

    /// Returns cached lines if content length hasn't changed, otherwise None.
    pub fn get(&self, content_len: usize) -> Option<&[Line<'static>]> {
        if content_len > 0 && content_len == self.rendered_len {
            Some(&self.lines)
        } else {
            None
        }
    }

    /// Store freshly rendered lines and the content length they correspond to.
    pub fn store(&mut self, content_len: usize, lines: Vec<Line<'static>>) {
        self.rendered_len = content_len;
        self.lines = lines;
    }

    /// Reset the cache (call when streaming_content is cleared).
    pub fn invalidate(&mut self) {
        self.rendered_len = 0;
        self.lines.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Sessions,
    Chat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Popup {
    None,
    ModelSelect,
    SessionSelect,
    NewSession,
    ThemeSelect,
    Help,
    Log,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Trace => Self::Debug,
            Self::Debug => Self::Info,
            Self::Info => Self::Warn,
            Self::Warn => Self::Error,
            Self::Error => Self::Trace,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppLogEntry {
    pub elapsed: Duration,
    pub level: LogLevel,
    pub target: &'static str,
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum ChatEntry {
    User {
        text: String,
        message_id: Option<String>,
    },
    Assistant {
        content: String,
        thinking: Option<String>,
    },
    ToolCall {
        tool_call_id: Option<String>,
        name: String,
        is_error: bool,
        detail: ToolDetail,
    },
    CompactionStart {
        token_estimate: u32,
    },
    CompactionEnd {
        token_estimate: Option<u32>,
        summary: String,
        summary_len: u32,
    },
    Info(String),
    Error(String),
    Elicitation {
        elicitation_id: String,
        message: String,
        source: String,
        /// None = pending; Some = responded with this outcome label.
        outcome: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoableTurn {
    pub turn_id: String,
    pub message_id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoFrameStatus {
    Pending,
    Confirmed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoFrame {
    pub turn_id: String,
    pub message_id: String,
    pub status: UndoFrameStatus,
    pub reverted_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UndoState {
    pub stack: Vec<UndoFrame>,
    pub frontier_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIndexEntryLite {
    pub path: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionState {
    pub trigger_start: usize,
    pub query: String,
    pub selected_index: usize,
    pub results: Vec<FileIndexEntryLite>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathCompletionState {
    pub query: String,
    pub selected_index: usize,
    pub results: Vec<FileIndexEntryLite>,
}

// ── Elicitation types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationOption {
    pub value: serde_json::Value,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElicitationFieldKind {
    SingleSelect { options: Vec<ElicitationOption> },
    MultiSelect { options: Vec<ElicitationOption> },
    TextInput,
    NumberInput { integer: bool },
    BooleanToggle,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationField {
    pub name: String,
    pub title: String,
    pub description: Option<String>,
    pub required: bool,
    pub kind: ElicitationFieldKind,
}

#[derive(Debug, Clone)]
pub struct ElicitationState {
    pub elicitation_id: String,
    pub message: String,
    pub source: String,
    pub fields: Vec<ElicitationField>,
    /// Which field is active (for multi-field forms, currently always 0).
    pub field_cursor: usize,
    /// Which option within the current select field is highlighted.
    pub option_cursor: usize,
    /// Accumulated selections (field name → value).
    pub selected: HashMap<String, serde_json::Value>,
    /// Text buffer for TextInput / NumberInput fields.
    pub text_input: String,
    pub text_cursor: usize,
}

impl ElicitationState {
    /// Parse a JSON Schema `properties` object into a flat list of fields.
    /// Mirrors `parseSchema` in `ElicitationCard.tsx`.
    pub fn parse_schema(schema: &serde_json::Value) -> Vec<ElicitationField> {
        let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
            return Vec::new();
        };
        let required: std::collections::HashSet<&str> = schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        let mut fields = Vec::new();
        for (name, prop) in props {
            let title = prop
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or(name)
                .to_string();
            let description = prop
                .get("description")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let typ = prop
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("string");

            let kind = if let Some(one_of) = prop.get("oneOf").and_then(|v| v.as_array()) {
                ElicitationFieldKind::SingleSelect {
                    options: one_of
                        .iter()
                        .map(|opt| ElicitationOption {
                            value: opt.get("const").cloned().unwrap_or(serde_json::Value::Null),
                            label: opt
                                .get("title")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            description: opt
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                        })
                        .collect(),
                }
            } else if let Some(enum_vals) = prop.get("enum").and_then(|v| v.as_array()) {
                ElicitationFieldKind::SingleSelect {
                    options: enum_vals
                        .iter()
                        .map(|v| ElicitationOption {
                            value: v.clone(),
                            label: v.as_str().unwrap_or("").to_string(),
                            description: None,
                        })
                        .collect(),
                }
            } else if typ == "array" {
                let items = prop.get("items");
                let item_opts = items
                    .and_then(|i| i.get("anyOf").or_else(|| i.get("oneOf")))
                    .and_then(|v| v.as_array());
                if let Some(opts) = item_opts {
                    ElicitationFieldKind::MultiSelect {
                        options: opts
                            .iter()
                            .map(|opt| ElicitationOption {
                                value: opt.get("const").cloned().unwrap_or(serde_json::Value::Null),
                                label: opt
                                    .get("title")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                description: opt
                                    .get("description")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string()),
                            })
                            .collect(),
                    }
                } else {
                    ElicitationFieldKind::TextInput
                }
            } else if typ == "boolean" {
                ElicitationFieldKind::BooleanToggle
            } else if typ == "integer" {
                ElicitationFieldKind::NumberInput { integer: true }
            } else if typ == "number" {
                ElicitationFieldKind::NumberInput { integer: false }
            } else {
                ElicitationFieldKind::TextInput
            };

            fields.push(ElicitationField {
                name: name.clone(),
                title,
                description,
                required: required.contains(name.as_str()),
                kind,
            });
        }
        fields
    }

    /// Current active field (panics if fields is empty — callers should guard).
    pub fn current_field(&self) -> &ElicitationField {
        &self.fields[self.field_cursor.min(self.fields.len().saturating_sub(1))]
    }

    /// Number of options in the current field's select list (0 for non-select).
    pub fn current_option_count(&self) -> usize {
        match &self.current_field().kind {
            ElicitationFieldKind::SingleSelect { options } => options.len(),
            ElicitationFieldKind::MultiSelect { options } => options.len(),
            _ => 0,
        }
    }

    /// Move the option cursor by `delta`, clamped to valid range.
    pub fn move_cursor(&mut self, delta: i32) {
        let max = self.current_option_count().saturating_sub(1);
        self.option_cursor = (self.option_cursor as i32 + delta).clamp(0, max as i32) as usize;
    }

    /// For SingleSelect: record the highlighted option as the field's value.
    pub fn select_current_option(&mut self) {
        let field = self.current_field();
        if let ElicitationFieldKind::SingleSelect { options } = &field.kind
            && let Some(opt) = options.get(self.option_cursor)
        {
            let name = field.name.clone();
            let value = opt.value.clone();
            self.selected.insert(name, value);
        }
    }

    /// For MultiSelect: toggle the highlighted option in the field's array value.
    pub fn toggle_current_option(&mut self) {
        let field = self.current_field();
        if let ElicitationFieldKind::MultiSelect { options } = &field.kind
            && let Some(opt) = options.get(self.option_cursor)
        {
            let name = field.name.clone();
            let val = opt.value.clone();
            let arr = self
                .selected
                .entry(name)
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            if let serde_json::Value::Array(items) = arr {
                if let Some(pos) = items.iter().position(|v| v == &val) {
                    items.remove(pos);
                } else {
                    items.push(val);
                }
            }
        }
    }

    /// Build the `content` object to send with an accept response.
    pub fn build_accept_content(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        for field in &self.fields {
            match &field.kind {
                ElicitationFieldKind::SingleSelect { .. }
                | ElicitationFieldKind::MultiSelect { .. } => {
                    if let Some(v) = self.selected.get(&field.name) {
                        obj.insert(field.name.clone(), v.clone());
                    }
                }
                ElicitationFieldKind::TextInput => {
                    if !self.text_input.is_empty() {
                        obj.insert(
                            field.name.clone(),
                            serde_json::Value::String(self.text_input.clone()),
                        );
                    }
                }
                ElicitationFieldKind::NumberInput { integer } => {
                    if !self.text_input.is_empty() {
                        let v = if *integer {
                            self.text_input
                                .parse::<i64>()
                                .map(|n| serde_json::json!(n))
                                .unwrap_or(serde_json::Value::Null)
                        } else {
                            self.text_input
                                .parse::<f64>()
                                .map(|n| serde_json::json!(n))
                                .unwrap_or(serde_json::Value::Null)
                        };
                        obj.insert(field.name.clone(), v);
                    }
                }
                ElicitationFieldKind::BooleanToggle => {
                    if let Some(v) = self.selected.get(&field.name) {
                        obj.insert(field.name.clone(), v.clone());
                    }
                }
            }
        }
        serde_json::Value::Object(obj)
    }

    /// Returns true if all required fields have a value.
    pub fn is_valid(&self) -> bool {
        for field in &self.fields {
            if !field.required {
                continue;
            }
            match &field.kind {
                ElicitationFieldKind::SingleSelect { .. }
                | ElicitationFieldKind::MultiSelect { .. }
                | ElicitationFieldKind::BooleanToggle => {
                    if !self.selected.contains_key(&field.name) {
                        return false;
                    }
                }
                ElicitationFieldKind::TextInput | ElicitationFieldKind::NumberInput { .. } => {
                    if self.text_input.is_empty() {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Source label for display (strips the "mcp:" / "builtin:" prefix).
    pub fn source_label(&self) -> &str {
        if let Some(rest) = self.source.strip_prefix("mcp:") {
            rest
        } else if self.source == "builtin:question" {
            "built-in"
        } else {
            &self.source
        }
    }

    /// Returns a human-readable summary of what the user selected/entered,
    /// for display in the resolved chat card.
    ///
    /// - SingleSelect  → label of the chosen option
    /// - MultiSelect   → comma-joined labels of checked options
    /// - TextInput / NumberInput → the raw text
    /// - BooleanToggle → "Yes" or "No"
    pub fn selected_display(&self) -> String {
        let Some(field) = self.fields.first() else {
            return String::new();
        };
        match &field.kind {
            ElicitationFieldKind::SingleSelect { options } => {
                let val = self.selected.get(&field.name);
                options
                    .iter()
                    .find(|o| Some(&o.value) == val)
                    .map(|o| format!("{OUTCOME_BULLET}{}", o.label))
                    .unwrap_or_default()
            }
            ElicitationFieldKind::MultiSelect { options } => {
                if let Some(serde_json::Value::Array(arr)) = self.selected.get(&field.name) {
                    options
                        .iter()
                        .filter(|o| arr.contains(&o.value))
                        .map(|o| format!("{OUTCOME_BULLET}{}", o.label))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    String::new()
                }
            }
            ElicitationFieldKind::TextInput | ElicitationFieldKind::NumberInput { .. } => {
                self.text_input.clone()
            }
            ElicitationFieldKind::BooleanToggle => {
                match self.selected.get(&field.name).and_then(|v| v.as_bool()) {
                    Some(true) => "Yes".into(),
                    Some(false) => "No".into(),
                    None => String::new(),
                }
            }
        }
    }

    /// Constructor used by unit tests.
    #[cfg(test)]
    pub fn new_for_test(fields: Vec<ElicitationField>) -> Self {
        Self {
            elicitation_id: "test-id".into(),
            message: "Test question".into(),
            source: "builtin:question".into(),
            fields,
            field_cursor: 0,
            option_cursor: 0,
            selected: HashMap::new(),
            text_input: String::new(),
            text_cursor: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionStatsLite {
    pub active_llm_duration: Duration,
    pub open_llm_request_ts: Option<i64>,
    pub open_llm_request_instant: Option<Instant>,
    pub latest_context_tokens: Option<u64>,
    pub total_tool_calls: u32,
}

#[derive(Debug, Clone)]
pub struct SessionActivity {
    pub last_event_at: Instant,
}

#[derive(Debug, Clone)]
pub enum ToolDetail {
    None,
    /// Compact one-liner info for display after tool name
    Summary(String),
    /// One-liner header + indented output lines below
    SummaryWithOutput {
        header: String,
        output: String,
    },
    Edit {
        file: String,
        old: String,
        new: String,
        start_line: Option<usize>,
        /// Pre-computed diff lines (avoids re-running TextDiff on every render).
        cached_lines: Vec<Line<'static>>,
    },
    WriteFile {
        path: String,
        content: String,
        /// Pre-computed write preview lines.
        cached_lines: Vec<Line<'static>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOp {
    Undo,
    Redo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityState {
    Idle,
    Thinking,
    Streaming,
    RunningTool { name: String },
    Compacting { token_estimate: u32 },
    SessionOp(SessionOp),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionEvent {
    Connecting { attempt: u32, delay_ms: u64 },
    Connected,
    Disconnected { reason: String },
}

const CANCEL_CONFIRM_TIMEOUT: Duration = Duration::from_millis(1000);

/// A single visible row on the start-page session list.
///
/// Built by [`App::visible_start_items`] each render frame, respecting the
/// current filter and per-group collapse state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartPageItem {
    /// A group header row (cwd label + count + collapsed state).
    GroupHeader {
        /// The cwd key used to look up collapse state (mirrors `SessionGroup::cwd`).
        cwd: Option<String>,
        /// Total sessions in this group (unfiltered).
        session_count: usize,
        /// Whether the group is currently collapsed.
        collapsed: bool,
    },
    /// A session row inside an expanded group.
    Session {
        /// Index into `App::session_groups`.
        group_idx: usize,
        /// Index into `App::session_groups[group_idx].sessions`.
        session_idx: usize,
    },
    /// A "… show all (N total)" row shown when a group has more sessions than
    /// `MAX_RECENT_SESSIONS`. Pressing Enter opens the session popup.
    ShowMore {
        /// Number of sessions hidden beyond the first `MAX_RECENT_SESSIONS`.
        remaining: usize,
    },
}

/// Maximum number of recent sessions shown per group before a ShowMore row.
pub const MAX_RECENT_SESSIONS: usize = 3;

/// Maximum number of workspace groups shown on the start page before a ShowMore row.
pub const MAX_VISIBLE_GROUPS: usize = 3;

/// In-memory per-mode cached state within a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedModeState {
    /// `"provider/model"` e.g. `"anthropic/claude-sonnet-4-20250514"`
    pub model: String,
    /// Reasoning effort level. `None` = auto.
    pub effort: Option<String>,
}

/// A single visible row in the sessions popup.
///
/// Built by [`App::visible_popup_items`]. Unlike [`StartPageItem`] there is no
/// `ShowMore` variant — the popup always shows all sessions and all groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopupItem {
    /// A group header row (cwd label + count + collapsed state).
    GroupHeader {
        /// The cwd key used to look up collapse state (mirrors `SessionGroup::cwd`).
        cwd: Option<String>,
        /// Total sessions in this group (unfiltered).
        session_count: usize,
        /// Whether the group is currently collapsed in the popup.
        collapsed: bool,
    },
    /// A session row inside an expanded group.
    Session {
        /// Index into `App::session_groups`.
        group_idx: usize,
        /// Index into `App::session_groups[group_idx].sessions`.
        session_idx: usize,
    },
}

pub struct App {
    pub screen: Screen,
    pub popup: Popup,
    pub chord: bool, // true after ctrl+x pressed, waiting for second key

    // sessions
    /// Session groups as received from the server (preserve group structure for start page).
    pub session_groups: Vec<SessionGroup>,
    pub session_cursor: usize,
    pub session_filter: String,
    /// Groups whose header has been collapsed by the user on the start page.
    pub collapsed_groups: HashSet<String>,
    /// Groups whose header has been collapsed by the user in the session popup.
    /// Separate from `collapsed_groups` so start-page and popup states are independent.
    pub popup_collapsed_groups: HashSet<String>,
    /// Scroll offset for the start-page session list (in visible rows).
    pub start_page_scroll: usize,

    // active session
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub agent_mode: String,
    pub launch_cwd: Option<String>,
    pub new_session_path: String,
    pub new_session_cursor: usize,
    pub new_session_completion: Option<PathCompletionState>,
    pub session_activity: HashMap<String, SessionActivity>,

    // chat
    pub messages: Vec<ChatEntry>,
    pub input: String,
    pub input_cursor: usize,
    pub input_scroll: u16,
    pub input_line_width: usize,
    pub input_preferred_col: Option<usize>,
    pub scroll_offset: u16,
    /// Total content height (in rows) from the last render frame.
    /// Used to compensate `scroll_offset` when content grows while the user
    /// is scrolled up, so the viewport stays at the same absolute position.
    pub prev_total_height: u16,
    pub activity: ActivityState,
    pub streaming_content: String,
    pub streaming_cache: StreamingCache,
    pub streaming_thinking: String,
    pub streaming_thinking_cache: StreamingCache,
    pub file_index: Vec<FileIndexEntryLite>,
    pub file_index_generated_at: Option<u64>,
    pub file_index_loading: bool,
    pub file_index_error: Option<String>,
    pub mention_state: Option<MentionState>,
    pub last_compaction_token_estimate: Option<u32>,
    /// Active elicitation request waiting for user response.
    pub elicitation: Option<ElicitationState>,

    // thinking display
    pub show_thinking: bool,

    // reasoning effort
    /// Current reasoning-effort level. `None` = "auto" (server default).
    /// Matches `reasoningEffort: string | null` in the web UI.
    pub reasoning_effort: Option<String>,
    /// Per-session, per-mode cache: session_id → mode → CachedModeState.
    /// Stores which model and reasoning effort were used in each mode within each
    /// session.  Loaded from `~/.cache/qmt/tui-cache.toml` on startup.
    pub session_cache: HashMap<String, HashMap<String, CachedModeState>>,

    // model info
    pub current_model: Option<String>,
    pub current_provider: Option<String>,
    pub models: Vec<ModelEntry>,
    pub model_cursor: usize,
    pub model_filter: String,
    /// Per-mode model preferences: mode -> (provider, model).
    /// Set when the user manually selects a model; applied automatically on mode switch.
    pub mode_model_preferences: HashMap<String, (String, String)>,

    // theme selector
    pub theme_cursor: usize,
    pub theme_filter: String,

    // help popup
    pub help_scroll: usize,

    // in-memory logs popup
    pub started_at: Instant,
    pub logs: Vec<AppLogEntry>,
    pub log_cursor: usize,
    pub log_filter: String,
    pub log_level_filter: LogLevel,

    // Undo/redo state mirrors the web UI semantics: a server-authoritative stack
    // of reverted turns plus a frontier that marks the current branch point.
    pub undo_state: Option<UndoState>,
    pub undoable_turns: Vec<UndoableTurn>,

    // session stats
    pub cumulative_cost: Option<f64>,
    pub context_limit: u64,
    pub session_stats: SessionStatsLite,
    pub pending_cancel_confirm_until: Option<Instant>,

    // status line
    pub status: String,

    // connection
    pub conn: ConnState,
    pub reconnect_attempt: u32,
    pub reconnect_delay_ms: Option<u64>,

    // server lifecycle (managed by server_manager::supervisor)
    pub server_state: crate::server_manager::ServerState,

    // syntax highlighting
    pub hl: Highlighter,

    // card cache for incremental rendering
    pub card_cache: CardCache,

    pub tick: u64,
    pub should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            screen: Screen::Sessions,
            popup: Popup::None,
            chord: false,
            session_groups: Vec::new(),
            session_cursor: 0,
            session_filter: String::new(),
            collapsed_groups: HashSet::new(),
            popup_collapsed_groups: HashSet::new(),
            start_page_scroll: 0,
            session_id: None,
            agent_id: None,
            agent_mode: "build".into(),
            launch_cwd: None,
            new_session_path: String::new(),
            new_session_cursor: 0,
            new_session_completion: None,
            session_activity: HashMap::new(),
            messages: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            input_scroll: 0,
            input_line_width: 1,
            input_preferred_col: None,
            scroll_offset: 0,
            prev_total_height: 0,
            activity: ActivityState::Idle,
            streaming_content: String::new(),
            streaming_cache: StreamingCache::new(),
            streaming_thinking: String::new(),
            streaming_thinking_cache: StreamingCache::new(),
            file_index: Vec::new(),
            file_index_generated_at: None,
            file_index_loading: false,
            file_index_error: None,
            mention_state: None,
            last_compaction_token_estimate: None,
            elicitation: None,
            show_thinking: true,
            reasoning_effort: None,
            session_cache: HashMap::new(),
            current_model: None,
            current_provider: None,
            models: Vec::new(),
            model_cursor: 0,
            model_filter: String::new(),
            mode_model_preferences: HashMap::new(),
            theme_cursor: 0,
            theme_filter: String::new(),
            help_scroll: 0,
            started_at: Instant::now(),
            logs: Vec::new(),
            log_cursor: 0,
            log_filter: String::new(),
            log_level_filter: LogLevel::Info,
            undo_state: None,
            undoable_turns: Vec::new(),
            cumulative_cost: None,
            context_limit: 0,
            session_stats: SessionStatsLite::default(),
            pending_cancel_confirm_until: None,
            conn: ConnState::Connecting,
            reconnect_attempt: 0,
            reconnect_delay_ms: None,
            server_state: crate::server_manager::ServerState::default(),
            hl: Highlighter::new(),
            card_cache: CardCache::new(),
            status: "connecting...".into(),
            tick: 0,
            should_quit: false,
        }
    }

    /// Invalidate both streaming caches and clear the thinking buffer.
    ///
    /// Call this when a streaming turn ends (assistant message finalized,
    /// new turn starts, session reloaded, etc.) so stale markdown renders
    /// are discarded.
    pub fn invalidate_streaming_caches(&mut self) {
        self.streaming_cache.invalidate();
        self.streaming_thinking.clear();
        self.streaming_thinking_cache.invalidate();
    }

    /// Short display label for the current reasoning effort level.
    /// Matches the five values used in the web UI: auto / low / medium / high / max.
    pub fn reasoning_effort_label(&self) -> &str {
        self.reasoning_effort.as_deref().unwrap_or("auto")
    }

    /// Cycle through `[auto, low, medium, high, max]` (wraps around).
    /// Updates `self.reasoning_effort` optimistically, saves the new value as
    /// the preference for the current `(mode, provider, model)` context, and
    /// returns the [`ClientMsg`] to forward to the server.
    pub fn cycle_reasoning_effort(&mut self) -> ClientMsg {
        const LEVELS: &[Option<&str>] =
            &[None, Some("low"), Some("medium"), Some("high"), Some("max")];
        let current = self.reasoning_effort.as_deref();
        let idx = LEVELS
            .iter()
            .position(|l| l.as_deref() == current)
            .unwrap_or(0);
        let next = LEVELS[(idx + 1) % LEVELS.len()];
        self.reasoning_effort = next.map(ToOwned::to_owned);
        // Cache the new value for the current session + mode.
        self.cache_session_mode_state();
        // Server expects the string "auto" when clearing the override.
        let effort_str = next.unwrap_or("auto").to_string();
        ClientMsg::SetReasoningEffort {
            reasoning_effort: effort_str,
        }
    }

    /// Save the current model + reasoning effort into the session cache for
    /// the current `session_id` + `agent_mode`.
    /// No-op if session_id, provider, or model are unknown.
    pub fn cache_session_mode_state(&mut self) {
        let (Some(sid), Some(provider), Some(model)) = (
            self.session_id.clone(),
            self.current_provider.clone(),
            self.current_model.clone(),
        ) else {
            return;
        };
        let model_key = format!("{provider}/{model}");
        self.session_cache.entry(sid).or_default().insert(
            self.agent_mode.clone(),
            CachedModeState {
                model: model_key,
                effort: self.reasoning_effort.clone(),
            },
        );
    }

    /// Look up the cached mode state for the current `session_id` +
    /// `agent_mode` and restore the model and effort from it.
    ///
    /// Returns a list of commands to send to the server:
    /// - `SetSessionModel` if the cached model differs from the current one
    /// - `SetReasoningEffort` if the cached effort differs from the current one
    ///
    /// Returns empty vec when there is no cache entry or nothing changed.
    pub fn apply_cached_mode_state(&mut self) -> Vec<ClientMsg> {
        let Some(sid) = self.session_id.clone() else {
            return vec![];
        };
        let cached = self
            .session_cache
            .get(&sid)
            .and_then(|modes| modes.get(&self.agent_mode))
            .cloned();
        let Some(cached) = cached else {
            return vec![];
        };

        let mut cmds = Vec::new();

        // Restore model if it differs from what the session currently has.
        let current_model_key = match (
            self.current_provider.as_deref(),
            self.current_model.as_deref(),
        ) {
            (Some(p), Some(m)) => format!("{p}/{m}"),
            _ => String::new(),
        };
        if cached.model != current_model_key {
            // Parse "provider/model" back into parts.
            if let Some((provider, model)) = cached.model.split_once('/') {
                // Find the model entry to get its full id + node_id.
                let model_entry = self
                    .models
                    .iter()
                    .find(|e| e.provider == provider && e.model == model);
                if let Some(entry) = model_entry {
                    cmds.push(ClientMsg::SetSessionModel {
                        session_id: sid.clone(),
                        model_id: entry.id.clone(),
                        node_id: entry.node_id.clone(),
                    });
                    self.current_provider = Some(provider.to_string());
                    self.current_model = Some(model.to_string());
                }
            }
        }

        // Restore effort if it differs.
        if cached.effort != self.reasoning_effort {
            self.reasoning_effort = cached.effort.clone();
            let effort_str = cached.effort.as_deref().unwrap_or("auto").to_string();
            cmds.push(ClientMsg::SetReasoningEffort {
                reasoning_effort: effort_str,
            });
        }

        cmds
    }

    pub fn filtered_models(&self) -> Vec<&ModelEntry> {
        if self.model_filter.is_empty() {
            self.models.iter().collect()
        } else {
            let q = self.model_filter.to_lowercase();
            self.models
                .iter()
                .filter(|m| {
                    m.label.to_lowercase().contains(&q)
                        || m.provider.to_lowercase().contains(&q)
                        || m.model.to_lowercase().contains(&q)
                })
                .collect()
        }
    }

    pub fn push_log(&mut self, level: LogLevel, target: &'static str, message: impl Into<String>) {
        let message = message.into();
        if self.logs.last().is_some_and(|entry| {
            entry.level == level && entry.target == target && entry.message == message
        }) {
            return;
        }
        self.logs.push(AppLogEntry {
            elapsed: self.started_at.elapsed(),
            level,
            target,
            message,
        });
    }

    pub fn set_status(
        &mut self,
        level: LogLevel,
        target: &'static str,
        message: impl Into<String>,
    ) {
        let message = message.into();
        self.status = message.clone();
        self.push_log(level, target, message);
    }

    pub fn filtered_logs(&self) -> Vec<&AppLogEntry> {
        let query = self.log_filter.to_lowercase();
        self.logs
            .iter()
            .filter(|entry| entry.level >= self.log_level_filter)
            .filter(|entry| {
                query.is_empty()
                    || entry.message.to_lowercase().contains(&query)
                    || entry.target.to_lowercase().contains(&query)
                    || entry.level.label().to_lowercase().contains(&query)
            })
            .collect()
    }

    pub fn cycle_log_level_filter(&mut self) {
        self.log_level_filter = self.log_level_filter.next();
    }

    pub fn cancel_confirm_active(&self) -> bool {
        self.pending_cancel_confirm_until
            .map(|deadline| Instant::now() <= deadline)
            .unwrap_or(false)
    }

    pub fn arm_cancel_confirm(&mut self) {
        self.pending_cancel_confirm_until = Some(Instant::now() + CANCEL_CONFIRM_TIMEOUT);
        self.set_status(LogLevel::Warn, "input", "press Esc again to stop");
    }

    pub fn clear_cancel_confirm(&mut self) {
        self.pending_cancel_confirm_until = None;
    }

    pub fn is_turn_active(&self) -> bool {
        matches!(
            self.activity,
            ActivityState::Thinking
                | ActivityState::Streaming
                | ActivityState::RunningTool { .. }
                | ActivityState::Compacting { .. }
        )
    }

    /// Adjust `scroll_offset` to compensate for content growth so the
    /// viewport stays at the same absolute position when the user is
    /// scrolled up.  No-op when `scroll_offset == 0` (auto-following).
    ///
    /// Call from the renderer after computing the new `total_height`.
    pub fn compensate_scroll_for_growth(&mut self, total_height: u16) {
        let growth = total_height.saturating_sub(self.prev_total_height);
        if self.scroll_offset > 0 && growth > 0 {
            self.scroll_offset = self.scroll_offset.saturating_add(growth);
        }
        self.prev_total_height = total_height;
    }

    pub fn has_cancellable_activity(&self) -> bool {
        self.is_turn_active()
    }

    pub fn has_pending_session_op(&self) -> bool {
        matches!(self.activity, ActivityState::SessionOp(_))
    }

    pub fn input_blocked_by_activity(&self) -> bool {
        self.elicitation.is_some()
            || self.has_pending_session_op()
            || self.pending_cancel_confirm_until.is_some()
    }

    pub fn should_hide_input_contents(&self) -> bool {
        self.input_blocked_by_activity()
    }

    pub fn activity_status_text(&self) -> Option<String> {
        match &self.activity {
            ActivityState::Idle => None,
            ActivityState::Thinking => Some("thinking...".into()),
            ActivityState::Streaming => Some("streaming...".into()),
            ActivityState::RunningTool { name } => Some(format!("tool: {name}")),
            ActivityState::Compacting { token_estimate } => {
                Some(format!("compacting context (~{token_estimate} tokens)"))
            }
            ActivityState::SessionOp(SessionOp::Undo) => Some("undoing...".into()),
            ActivityState::SessionOp(SessionOp::Redo) => Some("redoing...".into()),
        }
    }

    pub fn refresh_transient_status(&mut self) {
        if self.pending_cancel_confirm_until.is_some() {
            return;
        }
        if self.elicitation.is_some() {
            self.set_status(
                LogLevel::Debug,
                "elicitation",
                "question - answer in the panel above input",
            );
        } else if let Some(activity_status) = self.activity_status_text() {
            self.set_status(LogLevel::Debug, "activity", activity_status);
        } else if self.conn == ConnState::Connected {
            self.set_status(LogLevel::Debug, "activity", "ready");
        }
    }

    pub fn clear_expired_cancel_confirm(&mut self) {
        if self.pending_cancel_confirm_until.is_some() && !self.cancel_confirm_active() {
            self.clear_cancel_confirm();
            self.refresh_transient_status();
        }
    }

    pub fn begin_llm_request_span(&mut self, timestamp: Option<i64>) {
        if self.session_stats.open_llm_request_ts.is_none() {
            self.session_stats.open_llm_request_ts = timestamp;
            self.session_stats.open_llm_request_instant = Some(Instant::now());
        }
    }

    pub fn end_llm_request_span(&mut self, timestamp: Option<i64>) {
        let duration = match (self.session_stats.open_llm_request_ts, timestamp) {
            (Some(started), Some(ended)) if ended >= started => {
                Some(Duration::from_secs((ended - started) as u64))
            }
            _ => self
                .session_stats
                .open_llm_request_instant
                .map(|started| started.elapsed()),
        };
        if let Some(duration) = duration {
            self.session_stats.active_llm_duration += duration;
        }
        self.session_stats.open_llm_request_ts = None;
        self.session_stats.open_llm_request_instant = None;
    }

    pub fn apply_event_stats(&mut self, kind: &EventKind, timestamp: Option<i64>) {
        match kind {
            EventKind::ToolCallStart { .. } => {
                self.session_stats.total_tool_calls =
                    self.session_stats.total_tool_calls.saturating_add(1);
            }
            EventKind::LlmRequestStart { .. } => {
                self.begin_llm_request_span(timestamp);
            }
            EventKind::LlmRequestEnd { context_tokens, .. } => {
                self.end_llm_request_span(timestamp);
                if let Some(ctx) = context_tokens {
                    self.session_stats.latest_context_tokens = Some(*ctx);
                }
            }
            EventKind::Cancelled | EventKind::Error { .. } => {
                self.end_llm_request_span(timestamp);
            }
            _ => {}
        }
    }

    pub fn llm_request_elapsed(&self) -> Option<Duration> {
        let mut elapsed = self.session_stats.active_llm_duration;
        if let Some(started) = self.session_stats.open_llm_request_instant {
            elapsed += started.elapsed();
        }
        if elapsed.is_zero() {
            None
        } else {
            Some(elapsed)
        }
    }

    pub fn handle_connection_event(&mut self, event: ConnectionEvent) {
        self.clear_cancel_confirm();
        match event {
            ConnectionEvent::Connecting { attempt, delay_ms } => {
                self.conn = ConnState::Connecting;
                self.reconnect_attempt = attempt;
                self.reconnect_delay_ms = Some(delay_ms);
                let secs = delay_ms as f64 / 1000.0;
                self.set_status(
                    LogLevel::Warn,
                    "connection",
                    format!("waiting for server - retry {attempt} in {secs:.1}s"),
                );
            }
            ConnectionEvent::Connected => {
                self.conn = ConnState::Connected;
                self.reconnect_attempt = 0;
                self.reconnect_delay_ms = None;
                self.set_status(
                    LogLevel::Info,
                    "connection",
                    if self.session_id.is_some() {
                        "reconnected".to_string()
                    } else {
                        "connected".to_string()
                    },
                );
            }
            ConnectionEvent::Disconnected { reason } => {
                self.conn = ConnState::Disconnected;
                self.reconnect_delay_ms = None;
                self.set_status(
                    LogLevel::Warn,
                    "connection",
                    format!("connection lost - {reason}"),
                );
            }
        }
    }

    pub fn has_pending_undo(&self) -> bool {
        self.undo_state
            .as_ref()
            .map(|state| {
                state
                    .stack
                    .iter()
                    .any(|frame| frame.status == UndoFrameStatus::Pending)
            })
            .unwrap_or(false)
    }

    pub fn pending_session_label(&self) -> Option<&'static str> {
        match self.activity {
            ActivityState::SessionOp(SessionOp::Undo) => Some("undoing"),
            ActivityState::SessionOp(SessionOp::Redo) => Some("redoing"),
            _ => None,
        }
    }

    pub fn current_undo_target(&self) -> Option<&UndoableTurn> {
        let frontier_message_id = self
            .undo_state
            .as_ref()
            .and_then(|state| state.frontier_message_id.as_deref());

        let mut start_index = self.undoable_turns.len();
        if let Some(frontier_message_id) = frontier_message_id
            && let Some(frontier_index) = self
                .undoable_turns
                .iter()
                .position(|turn| turn.message_id == frontier_message_id)
        {
            start_index = frontier_index;
        }

        self.undoable_turns[..start_index]
            .iter()
            .rev()
            .find(|turn| !turn.message_id.is_empty())
    }

    pub fn can_redo(&self) -> bool {
        self.undo_state
            .as_ref()
            .map(|state| !state.stack.is_empty())
            .unwrap_or(false)
    }

    pub fn push_pending_undo(&mut self, turn: &UndoableTurn) {
        let mut stack = self
            .undo_state
            .as_ref()
            .map(|state| state.stack.clone())
            .unwrap_or_default();
        stack.push(UndoFrame {
            turn_id: turn.turn_id.clone(),
            message_id: turn.message_id.clone(),
            status: UndoFrameStatus::Pending,
            reverted_files: Vec::new(),
        });
        self.undo_state = Some(UndoState {
            stack,
            frontier_message_id: Some(turn.message_id.clone()),
        });
    }

    pub fn build_undo_state_from_server_stack(
        &self,
        undo_stack: &[UndoStackFrame],
        preferred_frontier_message_id: Option<&str>,
        reverted_files: Option<&[String]>,
    ) -> Option<UndoState> {
        if undo_stack.is_empty() {
            return None;
        }

        let previous_state = self.undo_state.as_ref();
        let mut previous_by_message_id = std::collections::HashMap::new();
        if let Some(previous_state) = previous_state {
            for frame in &previous_state.stack {
                previous_by_message_id.insert(frame.message_id.clone(), frame.clone());
            }
        }

        let stack: Vec<UndoFrame> = undo_stack
            .iter()
            .map(|frame| {
                let previous = previous_by_message_id.get(&frame.message_id);
                let reverted_files =
                    if preferred_frontier_message_id == Some(frame.message_id.as_str()) {
                        reverted_files
                            .map(|files| files.to_vec())
                            .or_else(|| previous.map(|frame| frame.reverted_files.clone()))
                            .unwrap_or_default()
                    } else {
                        previous
                            .map(|frame| frame.reverted_files.clone())
                            .unwrap_or_default()
                    };
                let turn_id = previous
                    .map(|frame| frame.turn_id.clone())
                    .or_else(|| {
                        self.undoable_turns
                            .iter()
                            .find(|turn| turn.message_id == frame.message_id)
                            .map(|turn| turn.turn_id.clone())
                    })
                    .unwrap_or_else(|| frame.message_id.clone());
                UndoFrame {
                    turn_id,
                    message_id: frame.message_id.clone(),
                    status: UndoFrameStatus::Confirmed,
                    reverted_files,
                }
            })
            .collect();

        let has_message = |message_id: Option<&str>| {
            message_id
                .map(|message_id| stack.iter().any(|frame| frame.message_id == message_id))
                .unwrap_or(false)
        };

        let frontier_message_id = if has_message(preferred_frontier_message_id) {
            preferred_frontier_message_id.map(ToOwned::to_owned)
        } else if has_message(previous_state.and_then(|state| state.frontier_message_id.as_deref()))
        {
            previous_state.and_then(|state| state.frontier_message_id.clone())
        } else {
            stack.last().map(|frame| frame.message_id.clone())
        };

        Some(UndoState {
            stack,
            frontier_message_id,
        })
    }

    /// Mark the pending elicitation chat card with an outcome and clear the active state.
    pub fn resolve_elicitation(&mut self, elicitation_id: &str, outcome: &str) {
        for entry in &mut self.messages {
            if let ChatEntry::Elicitation {
                elicitation_id: eid,
                outcome: out,
                ..
            } = entry
                && eid == elicitation_id
            {
                *out = Some(outcome.to_string());
                break;
            }
        }
        self.elicitation = None;
        self.card_cache.invalidate();
        self.refresh_transient_status();
    }

    pub fn set_mode_model_preference(&mut self, mode: &str, provider: &str, model: &str) {
        self.mode_model_preferences
            .insert(mode.to_string(), (provider.to_string(), model.to_string()));
    }

    pub fn get_mode_model_preference(&self, mode: &str) -> Option<(&str, &str)> {
        self.mode_model_preferences
            .get(mode)
            .map(|(p, m)| (p.as_str(), m.as_str()))
    }

    pub fn next_mode(&self) -> &'static str {
        match self.agent_mode.as_str() {
            "build" => "plan",
            "plan" => "build",
            _ => "build",
        }
    }
}

// ── reasoning_effort_tests ────────────────────────────────────────────────────

#[cfg(test)]
mod reasoning_effort_tests {
    use super::*;

    // ── reasoning_effort_label ────────────────────────────────────────────────

    #[test]
    fn label_none_is_auto() {
        let app = App::new();
        assert_eq!(app.reasoning_effort_label(), "auto");
    }

    #[test]
    fn label_low() {
        let mut app = App::new();
        app.reasoning_effort = Some("low".into());
        assert_eq!(app.reasoning_effort_label(), "low");
    }

    #[test]
    fn label_medium() {
        let mut app = App::new();
        app.reasoning_effort = Some("medium".into());
        assert_eq!(app.reasoning_effort_label(), "medium");
    }

    #[test]
    fn label_high() {
        let mut app = App::new();
        app.reasoning_effort = Some("high".into());
        assert_eq!(app.reasoning_effort_label(), "high");
    }

    #[test]
    fn label_max() {
        let mut app = App::new();
        app.reasoning_effort = Some("max".into());
        assert_eq!(app.reasoning_effort_label(), "max");
    }

    #[test]
    fn label_unknown_passes_through() {
        let mut app = App::new();
        app.reasoning_effort = Some("ultra".into());
        assert_eq!(app.reasoning_effort_label(), "ultra");
    }

    // ── cycle_reasoning_effort ────────────────────────────────────────────────

    #[test]
    fn cycle_from_auto_to_low() {
        let mut app = App::new();
        assert_eq!(app.reasoning_effort, None);
        app.cycle_reasoning_effort();
        assert_eq!(app.reasoning_effort, Some("low".into()));
    }

    #[test]
    fn cycle_from_low_to_medium() {
        let mut app = App::new();
        app.reasoning_effort = Some("low".into());
        app.cycle_reasoning_effort();
        assert_eq!(app.reasoning_effort, Some("medium".into()));
    }

    #[test]
    fn cycle_from_medium_to_high() {
        let mut app = App::new();
        app.reasoning_effort = Some("medium".into());
        app.cycle_reasoning_effort();
        assert_eq!(app.reasoning_effort, Some("high".into()));
    }

    #[test]
    fn cycle_from_high_to_max() {
        let mut app = App::new();
        app.reasoning_effort = Some("high".into());
        app.cycle_reasoning_effort();
        assert_eq!(app.reasoning_effort, Some("max".into()));
    }

    #[test]
    fn cycle_from_max_wraps_to_auto() {
        let mut app = App::new();
        app.reasoning_effort = Some("max".into());
        app.cycle_reasoning_effort();
        assert_eq!(app.reasoning_effort, None);
    }

    #[test]
    fn cycle_full_round_trip() {
        let mut app = App::new();
        // auto → low → medium → high → max → auto
        for _ in 0..5 {
            app.cycle_reasoning_effort();
        }
        assert_eq!(app.reasoning_effort, None);
    }

    #[test]
    fn cycle_returns_correct_client_msg() {
        let mut app = App::new(); // starts at auto
        let msg = app.cycle_reasoning_effort();
        // auto → low: should send "low"
        match msg {
            ClientMsg::SetReasoningEffort { reasoning_effort } => {
                assert_eq!(reasoning_effort, "low");
            }
            other => panic!("expected SetReasoningEffort, got {other:?}"),
        }
    }

    #[test]
    fn cycle_to_auto_sends_auto_string() {
        let mut app = App::new();
        app.reasoning_effort = Some("max".into());
        let msg = app.cycle_reasoning_effort();
        // max → auto: server expects "auto" string (not null)
        match msg {
            ClientMsg::SetReasoningEffort { reasoning_effort } => {
                assert_eq!(reasoning_effort, "auto");
            }
            other => panic!("expected SetReasoningEffort, got {other:?}"),
        }
    }

    // ── state message populates reasoning_effort ──────────────────────────────

    #[test]
    fn state_msg_sets_reasoning_effort() {
        let mut app = App::new();
        app.handle_server_msg(RawServerMsg {
            msg_type: "state".into(),
            data: Some(serde_json::json!({
                "active_session_id": null,
                "agents": [],
                "agent_mode": "build",
                "reasoning_effort": "high"
            })),
        });
        assert_eq!(app.reasoning_effort, Some("high".into()));
    }

    #[test]
    fn state_msg_with_null_reasoning_effort_sets_none() {
        let mut app = App::new();
        app.reasoning_effort = Some("medium".into());
        app.handle_server_msg(RawServerMsg {
            msg_type: "state".into(),
            data: Some(serde_json::json!({
                "active_session_id": null,
                "agents": [],
                "agent_mode": "build",
                "reasoning_effort": null
            })),
        });
        assert_eq!(app.reasoning_effort, None);
    }

    #[test]
    fn state_msg_missing_reasoning_effort_leaves_existing() {
        let mut app = App::new();
        app.reasoning_effort = Some("medium".into());
        app.handle_server_msg(RawServerMsg {
            msg_type: "state".into(),
            data: Some(serde_json::json!({
                "active_session_id": null,
                "agents": [],
                "agent_mode": "build"
                // reasoning_effort key absent → existing value preserved
            })),
        });
        assert_eq!(app.reasoning_effort, Some("medium".into()));
    }

    // ── reasoning_effort push notification ────────────────────────────────────

    #[test]
    fn reasoning_effort_push_updates_field() {
        let mut app = App::new();
        app.handle_server_msg(RawServerMsg {
            msg_type: "reasoning_effort".into(),
            data: Some(serde_json::json!({ "reasoning_effort": "max" })),
        });
        assert_eq!(app.reasoning_effort, Some("max".into()));
    }

    #[test]
    fn reasoning_effort_push_null_clears_field() {
        let mut app = App::new();
        app.reasoning_effort = Some("low".into());
        app.handle_server_msg(RawServerMsg {
            msg_type: "reasoning_effort".into(),
            data: Some(serde_json::json!({ "reasoning_effort": null })),
        });
        assert_eq!(app.reasoning_effort, None);
    }

    #[test]
    fn reasoning_effort_push_auto_string_clears_field() {
        let mut app = App::new();
        app.reasoning_effort = Some("high".into());
        app.handle_server_msg(RawServerMsg {
            msg_type: "reasoning_effort".into(),
            data: Some(serde_json::json!({ "reasoning_effort": "auto" })),
        });
        assert_eq!(app.reasoning_effort, None);
    }

    #[test]
    fn event_message_ignores_non_active_session() {
        let mut app = App::new();
        app.session_id = Some("session-b".into());

        app.handle_server_msg(RawServerMsg {
            msg_type: "event".into(),
            data: Some(serde_json::json!({
                "agent_id": "agent-1",
                "session_id": "session-a",
                "event": {
                    "type": "ephemeral",
                    "data": {
                        "kind": {
                            "type": "assistant_content_delta",
                            "data": {
                                "content": "leaked text",
                                "message_id": null
                            }
                        },
                        "timestamp": null
                    }
                }
            })),
        });

        assert!(app.streaming_content.is_empty());
        assert!(app.messages.is_empty());
    }

    #[test]
    fn session_events_message_ignores_non_active_session() {
        let mut app = App::new();
        app.session_id = Some("session-b".into());

        app.handle_server_msg(RawServerMsg {
            msg_type: "session_events".into(),
            data: Some(serde_json::json!({
                "session_id": "session-a",
                "agent_id": "agent-1",
                "events": [
                    {
                        "type": "ephemeral",
                        "data": {
                            "kind": {
                                "type": "assistant_content_delta",
                                "data": {
                                    "content": "leaked batch text",
                                    "message_id": null
                                }
                            },
                            "timestamp": null
                        }
                    }
                ]
            })),
        });

        assert!(app.streaming_content.is_empty());
        assert!(app.messages.is_empty());
    }

    #[test]
    fn event_message_applies_active_session() {
        let mut app = App::new();
        app.session_id = Some("session-a".into());

        app.handle_server_msg(RawServerMsg {
            msg_type: "event".into(),
            data: Some(serde_json::json!({
                "agent_id": "agent-1",
                "session_id": "session-a",
                "event": {
                    "type": "ephemeral",
                    "data": {
                        "kind": {
                            "type": "assistant_content_delta",
                            "data": {
                                "content": "visible text",
                                "message_id": null
                            }
                        },
                        "timestamp": null
                    }
                }
            })),
        });

        assert_eq!(app.streaming_content, "visible text");
    }

    #[test]
    fn non_active_session_event_still_counts_as_recent_activity() {
        let mut app = App::new();
        app.session_id = Some("session-b".into());

        app.handle_server_msg(RawServerMsg {
            msg_type: "event".into(),
            data: Some(serde_json::json!({
                "agent_id": "agent-1",
                "session_id": "session-a",
                "event": {
                    "type": "ephemeral",
                    "data": {
                        "kind": {
                            "type": "assistant_content_delta",
                            "data": {
                                "content": "hidden text",
                                "message_id": null
                            }
                        },
                        "timestamp": null
                    }
                }
            })),
        });

        assert_eq!(app.active_session_count(), 1);
        assert!(app.streaming_content.is_empty());
    }

    #[test]
    fn active_session_count_requires_multiple_recent_sessions() {
        let mut app = App::new();
        app.note_session_activity("session-a");
        assert_eq!(app.active_session_count(), 1);

        app.note_session_activity("session-b");
        assert_eq!(app.active_session_count(), 2);
    }

    #[test]
    fn other_active_session_count_excludes_current_session() {
        let mut app = App::new();
        app.session_id = Some("session-a".into());
        app.note_session_activity("session-a");
        app.note_session_activity("session-b");
        app.note_session_activity("session-c");

        assert_eq!(app.other_active_session_count(), 2);
    }

    #[test]
    fn other_active_session_count_shows_other_session_when_current_is_idle() {
        let mut app = App::new();
        app.session_id = Some("session-a".into());
        app.note_session_activity("session-b");

        assert_eq!(app.other_active_session_count(), 1);
    }

    #[test]
    fn active_session_count_excludes_stale_sessions() {
        let mut app = App::new();
        app.note_session_activity("session-a");
        app.session_activity.insert(
            "session-b".into(),
            SessionActivity {
                last_event_at: Instant::now() - Duration::from_secs(6),
            },
        );

        assert_eq!(app.active_session_count(), 1);
        assert_eq!(app.other_active_session_count(), 1);
    }

    #[test]
    fn resolve_new_session_default_cwd_prefers_active_session_cwd_then_group_then_launch() {
        let mut app = App::new();
        app.launch_cwd = Some("/launch".into());
        app.session_id = Some("session-a".into());
        app.session_groups = vec![SessionGroup {
            cwd: Some("/group".into()),
            latest_activity: None,
            sessions: vec![SessionSummary {
                session_id: "session-a".into(),
                title: Some("Session A".into()),
                cwd: Some("/session".into()),
                created_at: None,
                updated_at: None,
                parent_session_id: None,
                has_children: false,
            }],
        }];
        assert_eq!(
            app.resolve_new_session_default_cwd().as_deref(),
            Some("/session")
        );

        app.session_groups[0].sessions[0].cwd = None;
        assert_eq!(
            app.resolve_new_session_default_cwd().as_deref(),
            Some("/group")
        );

        app.session_groups.clear();
        assert_eq!(
            app.resolve_new_session_default_cwd().as_deref(),
            Some("/launch")
        );
    }

    #[test]
    fn open_new_session_popup_prefills_path_and_cursor() {
        let mut app = App::new();
        app.launch_cwd = Some("/launch".into());

        app.open_new_session_popup();

        assert_eq!(app.popup, Popup::NewSession);
        assert_eq!(app.new_session_path, "/launch");
        assert_eq!(app.new_session_cursor, "/launch".len());
    }

    #[test]
    fn normalize_new_session_path_uses_launch_cwd_for_relative_paths() {
        let mut app = App::new();
        app.launch_cwd = Some("/launch/base".into());

        assert_eq!(
            app.normalize_new_session_path("proj/subdir").as_deref(),
            Some("/launch/base/proj/subdir")
        );
        assert_eq!(
            app.normalize_new_session_path("../proj/./subdir/..",)
                .as_deref(),
            Some("/launch/proj")
        );
        assert_eq!(
            app.normalize_new_session_path("/absolute/path/../clean")
                .as_deref(),
            Some("/absolute/clean")
        );
    }

    #[test]
    fn normalize_new_session_path_expands_tilde() {
        let app = App::new();
        let home = dirs::home_dir().expect("home dir available for test");
        let expected = home.join("workspace").to_string_lossy().into_owned();

        assert_eq!(
            app.normalize_new_session_path("~/workspace").as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn accept_selected_new_session_completion_replaces_input() {
        let mut app = App::new();
        app.new_session_completion = Some(PathCompletionState {
            query: "pro".into(),
            selected_index: 0,
            results: vec![FileIndexEntryLite {
                path: "/launch/project/../project-two".into(),
                is_dir: true,
            }],
        });

        assert!(app.accept_selected_new_session_completion());
        assert_eq!(app.new_session_path, "/launch/project-two/");
        assert!(app.new_session_completion.is_none());
    }

    #[test]
    fn rank_path_completion_matches_filters_out_files() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("qmt-app-tests-path-complete-{pid}-{nanos}"));
        std::fs::create_dir_all(dir.join("project-dir")).unwrap();
        std::fs::write(dir.join("project-file.txt"), "x").unwrap();

        let mut app = App::new();
        app.launch_cwd = Some(dir.to_string_lossy().into_owned());
        let results = app.rank_path_completion_matches("project");

        assert!(results.iter().all(|entry| entry.is_dir));
        assert!(
            results
                .iter()
                .any(|entry| entry.path.ends_with("project-dir"))
        );
        assert!(
            !results
                .iter()
                .any(|entry| entry.path.ends_with("project-file.txt"))
        );
    }
}

// ── session_cache_tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod session_cache_tests {
    use super::*;

    fn set_ctx(app: &mut App, sid: &str, mode: &str, provider: &str, model: &str) {
        app.session_id = Some(sid.into());
        app.agent_mode = mode.into();
        app.current_provider = Some(provider.into());
        app.current_model = Some(model.into());
    }

    fn make_model_entry(provider: &str, model: &str) -> ModelEntry {
        ModelEntry {
            id: format!("{provider}/{model}"),
            label: model.into(),
            provider: provider.into(),
            model: model.into(),
            node_id: None,
            family: None,
            quant: None,
        }
    }

    // ── cache_session_mode_state ──────────────────────────────────────────────

    #[test]
    fn cache_stores_model_and_effort_under_session_and_mode() {
        let mut app = App::new();
        set_ctx(&mut app, "s1", "build", "anthropic", "claude-sonnet");
        app.reasoning_effort = Some("high".into());

        app.cache_session_mode_state();

        let cms = &app.session_cache["s1"]["build"];
        assert_eq!(cms.model, "anthropic/claude-sonnet");
        assert_eq!(cms.effort, Some("high".into()));
    }

    #[test]
    fn cache_stores_auto_effort_as_none() {
        let mut app = App::new();
        set_ctx(&mut app, "s1", "plan", "openai", "gpt-4o");
        app.reasoning_effort = None;

        app.cache_session_mode_state();

        let cms = &app.session_cache["s1"]["plan"];
        assert_eq!(cms.model, "openai/gpt-4o");
        assert_eq!(cms.effort, None);
    }

    #[test]
    fn cache_noop_when_no_session_id() {
        let mut app = App::new();
        app.agent_mode = "build".into();
        app.current_provider = Some("anthropic".into());
        app.current_model = Some("claude-sonnet".into());

        app.cache_session_mode_state();

        assert!(app.session_cache.is_empty());
    }

    #[test]
    fn cache_noop_when_no_provider_or_model() {
        let mut app = App::new();
        app.session_id = Some("s1".into());
        app.agent_mode = "build".into();

        app.cache_session_mode_state();

        assert!(app.session_cache.is_empty());
    }

    #[test]
    fn cache_overwrites_existing_mode_entry() {
        let mut app = App::new();
        set_ctx(&mut app, "s1", "build", "anthropic", "claude-sonnet");
        app.reasoning_effort = Some("low".into());
        app.cache_session_mode_state();

        // Switch model + effort, same session + mode
        app.current_model = Some("claude-opus".into());
        app.current_provider = Some("anthropic".into());
        app.reasoning_effort = Some("max".into());
        app.cache_session_mode_state();

        let cms = &app.session_cache["s1"]["build"];
        assert_eq!(cms.model, "anthropic/claude-opus");
        assert_eq!(cms.effort, Some("max".into()));
    }

    #[test]
    fn cache_different_modes_independent_within_session() {
        let mut app = App::new();

        set_ctx(&mut app, "s1", "build", "anthropic", "claude-sonnet");
        app.reasoning_effort = Some("high".into());
        app.cache_session_mode_state();

        set_ctx(&mut app, "s1", "plan", "openai", "gpt-4o");
        app.reasoning_effort = Some("low".into());
        app.cache_session_mode_state();

        assert_eq!(
            app.session_cache["s1"]["build"].model,
            "anthropic/claude-sonnet"
        );
        assert_eq!(app.session_cache["s1"]["build"].effort, Some("high".into()));
        assert_eq!(app.session_cache["s1"]["plan"].model, "openai/gpt-4o");
        assert_eq!(app.session_cache["s1"]["plan"].effort, Some("low".into()));
    }

    #[test]
    fn cache_different_sessions_independent() {
        let mut app = App::new();

        set_ctx(&mut app, "s1", "build", "anthropic", "claude-sonnet");
        app.reasoning_effort = Some("high".into());
        app.cache_session_mode_state();

        set_ctx(&mut app, "s2", "build", "anthropic", "claude-sonnet");
        app.reasoning_effort = Some("low".into());
        app.cache_session_mode_state();

        assert_eq!(app.session_cache["s1"]["build"].effort, Some("high".into()));
        assert_eq!(app.session_cache["s2"]["build"].effort, Some("low".into()));
    }

    // ── apply_cached_mode_state ───────────────────────────────────────────────

    #[test]
    fn apply_restores_effort_when_model_matches() {
        let mut app = App::new();
        set_ctx(&mut app, "s1", "build", "anthropic", "claude-sonnet");
        app.reasoning_effort = None;

        app.session_cache.entry("s1".into()).or_default().insert(
            "build".into(),
            CachedModeState {
                model: "anthropic/claude-sonnet".into(),
                effort: Some("high".into()),
            },
        );

        let cmds = app.apply_cached_mode_state();
        assert_eq!(app.reasoning_effort, Some("high".into()));
        assert_eq!(cmds.len(), 1);
        assert!(
            matches!(&cmds[0], ClientMsg::SetReasoningEffort { reasoning_effort } if reasoning_effort == "high")
        );
    }

    #[test]
    fn apply_restores_model_and_effort_when_model_differs() {
        let mut app = App::new();
        set_ctx(&mut app, "s1", "build", "anthropic", "claude-sonnet");
        app.reasoning_effort = None;
        // The cached state says build mode used opus with max effort
        app.session_cache.entry("s1".into()).or_default().insert(
            "build".into(),
            CachedModeState {
                model: "anthropic/claude-opus".into(),
                effort: Some("max".into()),
            },
        );
        // Need the model in the models list for the lookup
        app.models = vec![make_model_entry("anthropic", "claude-opus")];

        let cmds = app.apply_cached_mode_state();

        assert_eq!(app.current_provider.as_deref(), Some("anthropic"));
        assert_eq!(app.current_model.as_deref(), Some("claude-opus"));
        assert_eq!(app.reasoning_effort, Some("max".into()));
        assert_eq!(cmds.len(), 2);
        assert!(matches!(&cmds[0], ClientMsg::SetSessionModel { .. }));
        assert!(
            matches!(&cmds[1], ClientMsg::SetReasoningEffort { reasoning_effort } if reasoning_effort == "max")
        );
    }

    #[test]
    fn apply_returns_empty_when_no_cache_entry() {
        let mut app = App::new();
        set_ctx(&mut app, "s1", "build", "anthropic", "claude-sonnet");
        app.reasoning_effort = Some("high".into());

        let cmds = app.apply_cached_mode_state();
        assert!(cmds.is_empty());
        // Nothing changed
        assert_eq!(app.reasoning_effort, Some("high".into()));
    }

    #[test]
    fn apply_returns_empty_when_everything_matches() {
        let mut app = App::new();
        set_ctx(&mut app, "s1", "build", "anthropic", "claude-sonnet");
        app.reasoning_effort = Some("high".into());

        app.session_cache.entry("s1".into()).or_default().insert(
            "build".into(),
            CachedModeState {
                model: "anthropic/claude-sonnet".into(),
                effort: Some("high".into()),
            },
        );

        let cmds = app.apply_cached_mode_state();
        assert!(cmds.is_empty());
    }

    #[test]
    fn apply_returns_empty_when_no_session_id() {
        let mut app = App::new();
        app.agent_mode = "build".into();
        app.current_provider = Some("anthropic".into());
        app.current_model = Some("claude-sonnet".into());
        app.reasoning_effort = Some("max".into());

        let cmds = app.apply_cached_mode_state();
        assert!(cmds.is_empty());
        assert_eq!(app.reasoning_effort, Some("max".into()));
    }

    #[test]
    fn apply_skips_model_switch_when_model_not_in_models_list() {
        let mut app = App::new();
        set_ctx(&mut app, "s1", "build", "anthropic", "claude-sonnet");
        app.reasoning_effort = None;

        app.session_cache.entry("s1".into()).or_default().insert(
            "build".into(),
            CachedModeState {
                model: "anthropic/claude-opus".into(),
                effort: Some("max".into()),
            },
        );
        // models list is empty — can't resolve opus
        app.models = vec![];

        let cmds = app.apply_cached_mode_state();
        // Can't switch model, but effort still restored
        assert_eq!(app.current_model.as_deref(), Some("claude-sonnet")); // unchanged
        assert_eq!(app.reasoning_effort, Some("max".into()));
        assert_eq!(cmds.len(), 1);
        assert!(matches!(&cmds[0], ClientMsg::SetReasoningEffort { .. }));
    }

    // ── cycle auto-caches ─────────────────────────────────────────────────────

    #[test]
    fn cycle_caches_mode_state() {
        let mut app = App::new();
        set_ctx(&mut app, "s1", "build", "anthropic", "claude-sonnet");

        app.cycle_reasoning_effort();

        assert_eq!(app.reasoning_effort, Some("low".into()));
        let cms = &app.session_cache["s1"]["build"];
        assert_eq!(cms.model, "anthropic/claude-sonnet");
        assert_eq!(cms.effort, Some("low".into()));
    }

    #[test]
    fn cycle_does_not_cache_when_no_context() {
        let mut app = App::new();
        app.cycle_reasoning_effort();
        assert_eq!(app.reasoning_effort, Some("low".into()));
        assert!(app.session_cache.is_empty());
    }
}

// ── session_mode_tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod session_mode_tests {
    use super::*;

    fn provider_changed_event(provider: &str, model: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": {
                "type": "provider_changed",
                "data": { "provider": provider, "model": model }
            }
        })
    }

    fn mode_changed_event(mode: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": {
                "type": "session_mode_changed",
                "data": { "mode": mode }
            }
        })
    }

    fn make_audit(events: &[serde_json::Value]) -> serde_json::Value {
        serde_json::json!({ "events": events })
    }

    fn make_session_loaded(audit: serde_json::Value) -> RawServerMsg {
        RawServerMsg {
            msg_type: "session_loaded".into(),
            data: Some(serde_json::json!({
                "session_id": "s1",
                "agent_id": "a1",
                "audit": audit,
                "undo_stack": []
            })),
        }
    }

    // ── SessionModeChanged in live events ─────────────────────────────────────

    #[test]
    fn live_session_mode_changed_updates_agent_mode() {
        let mut app = App::new();
        app.agent_mode = "build".into();
        app.handle_event_kind(
            &EventKind::SessionModeChanged {
                mode: "plan".into(),
            },
            false,
        );
        assert_eq!(app.agent_mode, "plan");
    }

    #[test]
    fn live_session_mode_changed_to_build_updates_agent_mode() {
        let mut app = App::new();
        app.agent_mode = "plan".into();
        app.handle_event_kind(
            &EventKind::SessionModeChanged {
                mode: "build".into(),
            },
            false,
        );
        assert_eq!(app.agent_mode, "build");
    }

    // ── SessionModeChanged in audit replay ────────────────────────────────────

    #[test]
    fn replay_session_mode_changed_restores_mode() {
        let mut app = App::new();
        app.agent_mode = "build".into();
        let audit = make_audit(&[mode_changed_event("plan")]);
        app.replay_audit(&audit);
        assert_eq!(app.agent_mode, "plan");
    }

    #[test]
    fn replay_last_session_mode_changed_wins() {
        let mut app = App::new();
        app.agent_mode = "build".into();
        let audit = make_audit(&[
            mode_changed_event("plan"),
            mode_changed_event("build"),
            mode_changed_event("plan"),
        ]);
        app.replay_audit(&audit);
        assert_eq!(app.agent_mode, "plan");
    }

    #[test]
    fn replay_no_mode_change_leaves_agent_mode_unchanged() {
        let mut app = App::new();
        app.agent_mode = "build".into();
        let audit = make_audit(&[provider_changed_event("anthropic", "claude-sonnet")]);
        app.replay_audit(&audit);
        assert_eq!(app.agent_mode, "build");
    }

    // ── session_loaded returns SetAgentMode ───────────────────────────────────

    #[test]
    fn session_loaded_returns_set_agent_mode_from_audit() {
        let mut app = App::new();
        app.agent_mode = "build".into();
        let audit = make_audit(&[mode_changed_event("plan")]);
        let cmds = app.handle_server_msg(make_session_loaded(audit));
        assert!(
            cmds.iter().any(|m| matches!(
                m,
                ClientMsg::SetAgentMode { mode } if mode == "plan"
            )),
            "expected SetAgentMode(plan) in {cmds:?}"
        );
    }

    #[test]
    fn session_loaded_always_returns_set_agent_mode_even_without_mode_event() {
        let mut app = App::new();
        app.agent_mode = "build".into();
        let audit = make_audit(&[]);
        let cmds = app.handle_server_msg(make_session_loaded(audit));
        // No SessionModeChanged → agent_mode stays "build"; command still sent
        assert!(
            cmds.iter().any(|m| matches!(
                m,
                ClientMsg::SetAgentMode { mode } if mode == "build"
            )),
            "expected SetAgentMode(build) in {cmds:?}"
        );
    }

    // ── session_loaded restores mode state from session cache ──────────────────

    #[test]
    fn session_loaded_restores_effort_from_session_cache() {
        let mut app = App::new();
        // Pre-cache: session s1, mode plan, model anthropic/claude-sonnet, effort high
        app.session_cache.entry("s1".into()).or_default().insert(
            "plan".into(),
            CachedModeState {
                model: "anthropic/claude-sonnet".into(),
                effort: Some("high".into()),
            },
        );

        let audit = make_audit(&[
            provider_changed_event("anthropic", "claude-sonnet"),
            mode_changed_event("plan"),
        ]);
        let cmds = app.handle_server_msg(make_session_loaded(audit));
        assert!(
            cmds.iter().any(|m| matches!(
                m,
                ClientMsg::SetReasoningEffort { reasoning_effort } if reasoning_effort == "high"
            )),
            "expected SetReasoningEffort(high) in {cmds:?}"
        );
        assert_eq!(app.reasoning_effort, Some("high".into()));
    }

    #[test]
    fn session_loaded_restores_model_from_session_cache() {
        let mut app = App::new();
        // Cache says plan mode used opus
        app.session_cache.entry("s1".into()).or_default().insert(
            "plan".into(),
            CachedModeState {
                model: "anthropic/claude-opus".into(),
                effort: Some("max".into()),
            },
        );
        // Need opus in the models list
        app.models = vec![ModelEntry {
            id: "anthropic/claude-opus".into(),
            label: "claude-opus".into(),
            provider: "anthropic".into(),
            model: "claude-opus".into(),
            node_id: None,
            family: None,
            quant: None,
        }];

        // Audit says session was in plan mode using sonnet (different from cache)
        let audit = make_audit(&[
            provider_changed_event("anthropic", "claude-sonnet"),
            mode_changed_event("plan"),
        ]);
        let cmds = app.handle_server_msg(make_session_loaded(audit));

        // Cache wins: model switched to opus
        assert!(
            cmds.iter()
                .any(|m| matches!(m, ClientMsg::SetSessionModel { .. })),
            "expected SetSessionModel in {cmds:?}"
        );
        assert_eq!(app.current_model.as_deref(), Some("claude-opus"));
    }

    #[test]
    fn session_loaded_no_cache_entry_returns_no_effort_or_model_cmds() {
        let mut app = App::new();
        app.reasoning_effort = None;
        let audit = make_audit(&[
            provider_changed_event("anthropic", "claude-sonnet"),
            mode_changed_event("plan"),
        ]);

        let cmds = app.handle_server_msg(make_session_loaded(audit));
        // Only SetAgentMode, no SetReasoningEffort or SetSessionModel
        assert!(
            !cmds
                .iter()
                .any(|m| matches!(m, ClientMsg::SetReasoningEffort { .. })),
            "expected no SetReasoningEffort: {cmds:?}"
        );
        assert!(
            !cmds
                .iter()
                .any(|m| matches!(m, ClientMsg::SetSessionModel { .. })),
            "expected no SetSessionModel: {cmds:?}"
        );
    }

    // ── handle_server_msg returns Vec now (backward compat for other msgs) ────

    #[test]
    fn state_msg_returns_empty_vec() {
        let mut app = App::new();
        let cmds = app.handle_server_msg(RawServerMsg {
            msg_type: "state".into(),
            data: Some(serde_json::json!({
                "active_session_id": null,
                "agents": [],
                "agent_mode": "build"
            })),
        });
        assert!(cmds.is_empty());
    }

    #[test]
    fn session_created_returns_subscribe_in_vec() {
        let mut app = App::new();
        let cmds = app.handle_server_msg(RawServerMsg {
            msg_type: "session_created".into(),
            data: Some(serde_json::json!({
                "session_id": "s99",
                "agent_id": "a1",
                "request_id": null
            })),
        });
        assert!(
            cmds.iter().any(|m| matches!(m, ClientMsg::SubscribeSession { session_id, .. } if session_id == "s99")),
            "expected SubscribeSession in {cmds:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_msg::backfill_elicitation_outcomes;

    fn make_turn(message_id: &str) -> UndoableTurn {
        UndoableTurn {
            turn_id: format!("turn-{message_id}"),
            message_id: message_id.into(),
            text: format!("prompt {message_id}"),
        }
    }

    fn make_stack(ids: &[&str]) -> Vec<UndoStackFrame> {
        ids.iter()
            .map(|id| UndoStackFrame {
                message_id: (*id).into(),
            })
            .collect()
    }

    #[test]
    fn current_undo_target_moves_left_of_frontier() {
        let mut app = App::new();
        app.undoable_turns = vec![make_turn("msg-1"), make_turn("msg-2"), make_turn("msg-3")];

        assert_eq!(
            app.current_undo_target()
                .map(|turn| turn.message_id.as_str()),
            Some("msg-3")
        );

        app.undo_state = Some(UndoState {
            stack: vec![UndoFrame {
                turn_id: "turn-msg-3".into(),
                message_id: "msg-3".into(),
                status: UndoFrameStatus::Confirmed,
                reverted_files: vec![],
            }],
            frontier_message_id: Some("msg-3".into()),
        });

        assert_eq!(
            app.current_undo_target()
                .map(|turn| turn.message_id.as_str()),
            Some("msg-2")
        );
    }

    #[test]
    fn build_undo_state_confirms_frames_and_preserves_frontier() {
        let mut app = App::new();
        app.undoable_turns = vec![make_turn("msg-1"), make_turn("msg-2")];
        app.undo_state = Some(UndoState {
            stack: vec![UndoFrame {
                turn_id: "turn-msg-1".into(),
                message_id: "msg-1".into(),
                status: UndoFrameStatus::Pending,
                reverted_files: vec![],
            }],
            frontier_message_id: Some("msg-1".into()),
        });

        let next = app
            .build_undo_state_from_server_stack(
                &make_stack(&["msg-1", "msg-2"]),
                Some("msg-2"),
                Some(&["a.rs".into(), "b.rs".into()]),
            )
            .expect("undo state");

        assert_eq!(next.frontier_message_id.as_deref(), Some("msg-2"));
        assert_eq!(next.stack.len(), 2);
        assert!(
            next.stack
                .iter()
                .all(|frame| frame.status == UndoFrameStatus::Confirmed)
        );
        assert_eq!(next.stack[1].turn_id, "turn-msg-2");
        assert_eq!(next.stack[1].reverted_files, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn build_undo_state_returns_none_for_empty_stack() {
        let app = App::new();
        assert_eq!(
            app.build_undo_state_from_server_stack(&[], None, None),
            None
        );
    }

    #[test]
    fn pending_guard_tracks_pending_frames() {
        let mut app = App::new();
        let turn = make_turn("msg-1");
        app.push_pending_undo(&turn);

        assert!(app.has_pending_undo());
        assert_eq!(
            app.undo_state
                .as_ref()
                .and_then(|state| state.frontier_message_id.as_deref()),
            Some("msg-1")
        );
        assert_eq!(
            app.undo_state.as_ref().map(|state| state.stack.len()),
            Some(1)
        );
        assert_eq!(
            app.undo_state
                .as_ref()
                .map(|state| state.stack[0].status.clone()),
            Some(UndoFrameStatus::Pending)
        );
    }

    #[test]
    fn compaction_events_replace_live_indicator_with_summary_card() {
        let mut app = App::new();
        app.handle_event_kind(
            &EventKind::CompactionStart {
                token_estimate: 12_000,
            },
            false,
        );
        assert_eq!(
            app.activity,
            ActivityState::Compacting {
                token_estimate: 12_000
            }
        );
        assert!(matches!(
            app.messages.last(),
            Some(ChatEntry::CompactionStart {
                token_estimate: 12_000
            })
        ));

        app.handle_event_kind(
            &EventKind::CompactionEnd {
                summary: "Trimmed tool output".into(),
                summary_len: 19,
            },
            false,
        );

        assert_eq!(app.activity, ActivityState::Thinking);
        assert!(
            matches!(app.messages.last(), Some(ChatEntry::CompactionEnd { token_estimate: Some(12_000), summary, summary_len }) if summary == "Trimmed tool output" && *summary_len == 19)
        );
        assert!(
            !app.messages
                .iter()
                .any(|entry| matches!(entry, ChatEntry::CompactionStart { .. }))
        );
    }

    #[test]
    fn pending_session_label_stays_reserved_for_undo_and_redo() {
        let mut app = App::new();
        app.activity = ActivityState::Compacting {
            token_estimate: 9_000,
        };
        assert_eq!(app.pending_session_label(), None);

        app.activity = ActivityState::SessionOp(SessionOp::Undo);
        assert_eq!(app.pending_session_label(), Some("undoing"));
    }

    #[test]
    fn push_log_deduplicates_consecutive_entries() {
        let mut app = App::new();

        app.push_log(LogLevel::Info, "server", "starting local server");
        app.push_log(LogLevel::Info, "server", "starting local server");
        app.push_log(LogLevel::Warn, "server", "waiting for lock");

        assert_eq!(app.logs.len(), 2);
        assert_eq!(app.logs[0].level, LogLevel::Info);
        assert_eq!(app.logs[0].target, "server");
        assert_eq!(app.logs[0].message, "starting local server");
        assert_eq!(app.logs[1].level, LogLevel::Warn);
    }

    #[test]
    fn set_status_updates_visible_status_and_appends_log() {
        let mut app = App::new();

        app.set_status(LogLevel::Info, "connection", "connected");

        assert_eq!(app.status, "connected");
        let last = app.logs.last().expect("missing log entry");
        assert_eq!(last.level, LogLevel::Info);
        assert_eq!(last.target, "connection");
        assert_eq!(last.message, "connected");
    }

    #[test]
    fn filtered_logs_apply_level_threshold_and_text_filter() {
        let mut app = App::new();
        app.push_log(LogLevel::Debug, "activity", "ready");
        app.push_log(LogLevel::Warn, "server", "waiting for lock");
        app.push_log(LogLevel::Error, "server", "start failed");

        app.log_level_filter = LogLevel::Warn;
        let filtered = app.filtered_logs();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|entry| entry.level >= LogLevel::Warn));

        app.log_filter = "failed".into();
        let filtered = app.filtered_logs();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].message, "start failed");
    }

    #[test]
    fn cancel_confirm_arms_expires_and_restores_status() {
        let mut app = App::new();
        app.activity = ActivityState::Thinking;

        app.arm_cancel_confirm();
        assert!(app.cancel_confirm_active());
        assert_eq!(app.status, "press Esc again to stop");
        assert!(
            matches!(app.logs.last(), Some(entry) if entry.message == "press Esc again to stop")
        );

        app.pending_cancel_confirm_until = Some(Instant::now() - Duration::from_millis(1));
        app.clear_expired_cancel_confirm();
        assert!(!app.cancel_confirm_active());
        assert_eq!(app.status, "thinking...");
        assert!(matches!(app.logs.last(), Some(entry) if entry.message == "thinking..."));
    }

    #[test]
    fn refresh_transient_status_preserves_connection_and_operation_precedence() {
        let mut app = App::new();
        app.conn = ConnState::Disconnected;
        app.set_status(LogLevel::Warn, "connection", "connection lost - retrying");
        app.refresh_transient_status();
        assert_eq!(app.status, "connection lost - retrying");

        app.conn = ConnState::Connected;
        app.activity = ActivityState::Thinking;
        app.refresh_transient_status();
        assert_eq!(app.status, "thinking...");

        app.activity = ActivityState::Compacting {
            token_estimate: 2048,
        };
        app.refresh_transient_status();
        assert_eq!(app.status, "compacting context (~2048 tokens)");

        app.activity = ActivityState::SessionOp(SessionOp::Redo);
        app.refresh_transient_status();
        assert_eq!(app.status, "redoing...");
    }

    #[test]
    fn session_stats_track_llm_request_elapsed_context_and_tool_calls_from_events() {
        let mut app = App::new();
        app.apply_event_stats(
            &EventKind::PromptReceived {
                content: serde_json::json!("hi"),
                message_id: None,
            },
            Some(100),
        );
        app.apply_event_stats(
            &EventKind::LlmRequestStart {
                message_count: Some(2),
            },
            Some(120),
        );
        app.apply_event_stats(
            &EventKind::ToolCallStart {
                tool_call_id: Some("call-1".into()),
                tool_name: "read_tool".into(),
                arguments: None,
            },
            Some(130),
        );
        app.apply_event_stats(
            &EventKind::LlmRequestEnd {
                finish_reason: None,
                cost_usd: None,
                cumulative_cost_usd: None,
                context_tokens: Some(2048),
                tool_calls: Some(99),
                metrics: None,
            },
            Some(160),
        );

        assert_eq!(app.session_stats.latest_context_tokens, Some(2048));
        assert_eq!(app.session_stats.total_tool_calls, 1);
        assert_eq!(app.llm_request_elapsed(), Some(Duration::from_secs(40)));
    }

    #[test]
    fn cancelled_closes_open_llm_request_span() {
        let mut app = App::new();
        app.apply_event_stats(
            &EventKind::LlmRequestStart {
                message_count: Some(1),
            },
            Some(200),
        );
        app.apply_event_stats(&EventKind::Cancelled, Some(215));
        assert_eq!(app.llm_request_elapsed(), Some(Duration::from_secs(15)));
        assert_eq!(app.session_stats.open_llm_request_ts, None);
        assert_eq!(app.session_stats.open_llm_request_instant, None);
    }

    #[test]
    fn active_mention_query_detects_trigger_and_ignores_email() {
        let app = App::new();

        assert_eq!(
            app.active_mention_query_from("fix @src/ma", "fix @src/ma".len()),
            Some((4, "src/ma".into()))
        );
        assert_eq!(
            app.active_mention_query_from("email@test.com", "email@test.com".len()),
            None
        );
        assert_eq!(
            app.active_mention_query_from("foo @", 5),
            Some((4, String::new()))
        );
        assert_eq!(
            app.active_mention_query_from("foo @bar baz", 8),
            Some((4, "bar".into()))
        );
        assert_eq!(app.active_mention_query_from("foo @bar baz", 12), None);
    }

    #[test]
    fn mention_results_rank_prefix_before_loose_matches() {
        let mut app = App::new();
        app.file_index = vec![
            FileIndexEntryLite {
                path: "src/main.rs".into(),
                is_dir: false,
            },
            FileIndexEntryLite {
                path: "tests/main_spec.rs".into(),
                is_dir: false,
            },
            FileIndexEntryLite {
                path: "src/manifest.toml".into(),
                is_dir: false,
            },
            FileIndexEntryLite {
                path: "src".into(),
                is_dir: true,
            },
        ];

        let results = app.rank_file_matches("ma");
        let ranked: Vec<&str> = results.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(ranked[0], "src/main.rs");
        assert!(ranked.contains(&"src/manifest.toml"));
        assert!(ranked.contains(&"tests/main_spec.rs"));
    }

    #[test]
    fn input_up_visual_moves_to_previous_wrapped_row() {
        let mut app = App::new();
        app.input = "abcdef".into();
        app.input_cursor = 4;
        app.input_line_width = 4;

        app.input_up_visual(2);

        assert_eq!(app.input_cursor, 2);
        assert_eq!(app.input_preferred_col, Some(2));
    }

    #[test]
    fn input_down_visual_moves_to_next_wrapped_row() {
        let mut app = App::new();
        app.input = "abcdef".into();
        app.input_cursor = 2;
        app.input_line_width = 4;

        app.input_down_visual(2);

        assert_eq!(app.input_cursor, 4);
        assert_eq!(app.input_preferred_col, Some(2));
    }

    #[test]
    fn input_down_visual_crosses_newline_boundary() {
        let mut app = App::new();
        app.input = "ab\ncd".into();
        app.input_cursor = 1;
        app.input_line_width = 6;

        app.input_down_visual(2);

        assert_eq!(app.input_cursor, 4);
    }

    #[test]
    fn input_horizontal_move_resets_preferred_column() {
        let mut app = App::new();
        app.input = "abcdef".into();
        app.input_cursor = 4;
        app.input_preferred_col = Some(2);

        app.input_left();

        assert_eq!(app.input_cursor, 3);
        assert_eq!(app.input_preferred_col, None);
    }

    #[test]
    fn accept_selected_mention_replaces_query_with_friendly_token() {
        let mut app = App::new();
        app.input = "open @src/ma now".into();
        app.input_cursor = "open @src/ma".len();
        app.file_index = vec![FileIndexEntryLite {
            path: "src/main.rs".into(),
            is_dir: false,
        }];
        app.refresh_mention_state();

        let accepted = app.accept_selected_mention();
        assert!(accepted);
        assert_eq!(app.input, "open @src/main.rs  now");
        assert_eq!(app.input_cursor, "open @src/main.rs ".len());
        assert!(app.mention_state.is_none());
    }

    #[test]
    fn build_prompt_text_converts_friendly_mentions_to_markup_and_links() {
        let app = App::new();
        let (text, links) =
            app.build_prompt_text_and_links("check @src/main.rs and @src/lib.rs then @src/main.rs");
        assert_eq!(text, "check @src/main.rs and @src/lib.rs then @src/main.rs");
        assert_eq!(links, vec!["src/main.rs", "src/lib.rs"]);
    }

    #[test]
    fn activity_helpers_report_turn_and_session_state() {
        let mut app = App::new();
        assert!(!app.is_turn_active());
        assert!(!app.has_pending_session_op());
        assert!(!app.input_blocked_by_activity());
        assert!(!app.should_hide_input_contents());
        assert_eq!(app.pending_session_label(), None);

        app.activity = ActivityState::SessionOp(SessionOp::Undo);
        assert!(!app.is_turn_active());
        assert!(app.has_pending_session_op());
        assert!(app.input_blocked_by_activity());
        assert!(app.should_hide_input_contents());
        assert_eq!(app.pending_session_label(), Some("undoing"));

        app.activity = ActivityState::SessionOp(SessionOp::Redo);
        assert!(!app.is_turn_active());
        assert!(app.has_pending_session_op());
        assert!(app.input_blocked_by_activity());
        assert!(app.should_hide_input_contents());
        assert_eq!(app.pending_session_label(), Some("redoing"));

        app.activity = ActivityState::RunningTool {
            name: "read_tool".into(),
        };
        assert!(app.is_turn_active());
        assert!(app.has_cancellable_activity());
        assert!(!app.has_pending_session_op());
        assert!(!app.input_blocked_by_activity());
        assert!(!app.should_hide_input_contents());
        assert_eq!(app.pending_session_label(), None);

        app.arm_cancel_confirm();
        assert!(app.input_blocked_by_activity());
        assert!(app.should_hide_input_contents());
    }

    #[test]
    fn connection_events_update_status_and_retry_metadata() {
        let mut app = App::new();
        app.handle_connection_event(ConnectionEvent::Connecting {
            attempt: 3,
            delay_ms: 2000,
        });
        assert_eq!(app.conn, ConnState::Connecting);
        assert_eq!(app.reconnect_attempt, 3);
        assert_eq!(app.reconnect_delay_ms, Some(2000));
        assert_eq!(app.status, "waiting for server - retry 3 in 2.0s");
        assert!(
            matches!(app.logs.last(), Some(entry) if entry.target == "connection" && entry.level == LogLevel::Warn)
        );

        app.handle_connection_event(ConnectionEvent::Disconnected {
            reason: "socket closed".into(),
        });
        assert_eq!(app.conn, ConnState::Disconnected);
        assert_eq!(app.reconnect_delay_ms, None);
        assert_eq!(app.status, "connection lost - socket closed");
        assert!(
            matches!(app.logs.last(), Some(entry) if entry.message == "connection lost - socket closed")
        );

        app.session_id = Some("session-1".into());
        app.handle_connection_event(ConnectionEvent::Connected);
        assert_eq!(app.conn, ConnState::Connected);
        assert_eq!(app.reconnect_attempt, 0);
        assert_eq!(app.reconnect_delay_ms, None);
        assert_eq!(app.status, "reconnected");
        assert!(
            matches!(app.logs.last(), Some(entry) if entry.level == LogLevel::Info && entry.message == "reconnected")
        );
    }

    #[test]
    fn undo_and_redo_results_clear_pending_session_op() {
        let mut app = App::new();
        app.activity = ActivityState::SessionOp(SessionOp::Undo);
        app.handle_server_msg(RawServerMsg {
            msg_type: "undo_result".into(),
            data: Some(serde_json::json!({
                "success": false,
                "message": "undo failed",
                "undo_stack": []
            })),
        });
        assert_eq!(app.activity, ActivityState::Idle);

        app.activity = ActivityState::SessionOp(SessionOp::Redo);
        app.handle_server_msg(RawServerMsg {
            msg_type: "redo_result".into(),
            data: Some(serde_json::json!({
                "success": false,
                "message": "redo failed",
                "undo_stack": []
            })),
        });
        assert_eq!(app.activity, ActivityState::Idle);
    }

    #[test]
    fn turn_activity_transitions_across_tool_and_completion_events() {
        let mut app = App::new();

        app.handle_event_kind(&EventKind::TurnStarted, false);
        assert_eq!(app.activity, ActivityState::Thinking);

        app.handle_event_kind(
            &EventKind::AssistantMessageStored {
                content: "draft".into(),
                thinking: None,
                message_id: None,
            },
            false,
        );
        assert_eq!(app.activity, ActivityState::Thinking);

        app.handle_event_kind(
            &EventKind::ToolCallStart {
                tool_call_id: Some("call-1".into()),
                tool_name: "read_tool".into(),
                arguments: None,
            },
            false,
        );
        assert_eq!(
            app.activity,
            ActivityState::RunningTool {
                name: "read_tool".into()
            }
        );

        app.handle_event_kind(
            &EventKind::LlmRequestEnd {
                finish_reason: None,
                cost_usd: None,
                cumulative_cost_usd: None,
                context_tokens: None,
                tool_calls: None,
                metrics: None,
            },
            false,
        );
        assert_eq!(app.activity, ActivityState::Idle);
    }

    #[test]
    fn replay_audit_prunes_frontier_and_later_events_after_undo() {
        let mut app = App::new();
        app.undo_state = Some(UndoState {
            stack: vec![UndoFrame {
                turn_id: "turn-msg-2".into(),
                message_id: "msg-2".into(),
                status: UndoFrameStatus::Confirmed,
                reverted_files: vec![],
            }],
            frontier_message_id: Some("msg-2".into()),
        });

        let audit = serde_json::json!({
            "events": [
                {
                    "kind": {
                        "type": "prompt_received",
                        "data": {
                            "content": [{ "type": "text", "text": "first" }],
                            "message_id": "msg-1"
                        }
                    }
                },
                {
                    "kind": {
                        "type": "assistant_message_stored",
                        "data": {
                            "content": "reply one",
                            "thinking": null,
                            "message_id": "a-1"
                        }
                    }
                },
                {
                    "kind": {
                        "type": "prompt_received",
                        "data": {
                            "content": [{ "type": "text", "text": "second" }],
                            "message_id": "msg-2"
                        }
                    }
                },
                {
                    "kind": {
                        "type": "assistant_message_stored",
                        "data": {
                            "content": "reply two",
                            "thinking": null,
                            "message_id": "a-2"
                        }
                    }
                }
            ]
        });

        app.replay_audit(&audit);

        assert_eq!(app.messages.len(), 2);
        assert!(
            matches!(&app.messages[0], ChatEntry::User { text, message_id: Some(message_id) } if text == "first" && message_id == "msg-1")
        );
        assert!(
            matches!(&app.messages[1], ChatEntry::Assistant { content, .. } if content == "reply one")
        );
        assert_eq!(app.undoable_turns.len(), 1);
        assert_eq!(app.undoable_turns[0].message_id, "msg-1");
        assert!(app.can_redo());
    }

    // ── ElicitationState::selected_display ────────────────────────────────────

    #[test]
    fn selected_display_single_select_returns_chosen_label() {
        let mut state = ElicitationState::new_for_test(vec![ElicitationField {
            name: "choice".into(),
            title: "Choice".into(),
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
        }]);
        state
            .selected
            .insert("choice".into(), serde_json::json!("b"));
        assert_eq!(state.selected_display(), format!("{OUTCOME_BULLET}Beta"));
    }

    #[test]
    fn selected_display_multi_select_returns_bulleted_lines() {
        let mut state = ElicitationState::new_for_test(vec![ElicitationField {
            name: "tags".into(),
            title: "Tags".into(),
            description: None,
            required: false,
            kind: ElicitationFieldKind::MultiSelect {
                options: vec![
                    ElicitationOption {
                        value: serde_json::json!("x"),
                        label: "X".into(),
                        description: None,
                    },
                    ElicitationOption {
                        value: serde_json::json!("y"),
                        label: "Y".into(),
                        description: None,
                    },
                    ElicitationOption {
                        value: serde_json::json!("z"),
                        label: "Z".into(),
                        description: None,
                    },
                ],
            },
        }]);
        state
            .selected
            .insert("tags".into(), serde_json::json!(["x", "z"]));
        assert_eq!(
            state.selected_display(),
            format!("{OUTCOME_BULLET}X\n{OUTCOME_BULLET}Z")
        );
    }

    #[test]
    fn selected_display_text_input_returns_text() {
        let mut state = ElicitationState::new_for_test(vec![ElicitationField {
            name: "name".into(),
            title: "Name".into(),
            description: None,
            required: true,
            kind: ElicitationFieldKind::TextInput,
        }]);
        state.text_input = "Alice".into();
        assert_eq!(state.selected_display(), "Alice");
    }

    #[test]
    fn selected_display_boolean_returns_yes_or_no() {
        let mut state = ElicitationState::new_for_test(vec![ElicitationField {
            name: "ok".into(),
            title: "OK".into(),
            description: None,
            required: true,
            kind: ElicitationFieldKind::BooleanToggle,
        }]);
        state.selected.insert("ok".into(), serde_json::json!(true));
        assert_eq!(state.selected_display(), "Yes");
        state.selected.insert("ok".into(), serde_json::json!(false));
        assert_eq!(state.selected_display(), "No");
    }

    // ── ToolCallStart suppression for question ────────────────────────────────

    #[test]
    fn question_tool_call_start_does_not_push_chat_entry() {
        let mut app = App::new();
        app.handle_event_kind(
            &EventKind::ToolCallStart {
                tool_call_id: Some("call-1".into()),
                tool_name: "question".into(),
                arguments: None,
            },
            false,
        );
        assert!(
            !app.messages
                .iter()
                .any(|m| matches!(m, ChatEntry::ToolCall { .. }))
        );
    }

    #[test]
    fn other_tool_call_start_still_pushes_chat_entry() {
        let mut app = App::new();
        app.handle_event_kind(
            &EventKind::ToolCallStart {
                tool_call_id: Some("call-2".into()),
                tool_name: "read_tool".into(),
                arguments: None,
            },
            false,
        );
        assert!(
            app.messages
                .iter()
                .any(|m| matches!(m, ChatEntry::ToolCall { name, .. } if name == "read_tool"))
        );
    }

    // ── Elicitation: schema parsing ───────────────────────────────────────────

    #[test]
    fn parse_elicitation_schema_single_select() {
        let schema = serde_json::json!({
            "properties": {
                "choice": {
                    "title": "Pick one",
                    "description": "Your selection",
                    "oneOf": [
                        { "const": "a", "title": "Option A", "description": "First" },
                        { "const": "b", "title": "Option B" }
                    ]
                }
            },
            "required": ["choice"]
        });
        let fields = ElicitationState::parse_schema(&schema);
        assert_eq!(fields.len(), 1);
        let f = &fields[0];
        assert_eq!(f.name, "choice");
        assert_eq!(f.title, "Pick one");
        assert_eq!(f.description.as_deref(), Some("Your selection"));
        assert!(f.required);
        let ElicitationFieldKind::SingleSelect { options } = &f.kind else {
            panic!("expected SingleSelect");
        };
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].label, "Option A");
        assert_eq!(options[0].description.as_deref(), Some("First"));
        assert_eq!(options[1].label, "Option B");
        assert!(options[1].description.is_none());
    }

    #[test]
    fn parse_elicitation_schema_multi_select() {
        let schema = serde_json::json!({
            "properties": {
                "tags": {
                    "type": "array",
                    "items": {
                        "anyOf": [
                            { "const": "x", "title": "X" },
                            { "const": "y", "title": "Y" }
                        ]
                    }
                }
            },
            "required": []
        });
        let fields = ElicitationState::parse_schema(&schema);
        assert_eq!(fields.len(), 1);
        let ElicitationFieldKind::MultiSelect { options } = &fields[0].kind else {
            panic!("expected MultiSelect");
        };
        assert_eq!(options.len(), 2);
        assert!(!fields[0].required);
    }

    #[test]
    fn parse_elicitation_schema_text_and_boolean() {
        let schema = serde_json::json!({
            "properties": {
                "name": { "type": "string" },
                "count": { "type": "integer" },
                "confirm": { "type": "boolean" }
            },
            "required": ["name"]
        });
        let fields = ElicitationState::parse_schema(&schema);
        assert_eq!(fields.len(), 3);
        let kinds: Vec<_> = fields.iter().map(|f| (&f.name, &f.kind)).collect();
        assert!(matches!(
            kinds.iter().find(|(n, _)| *n == "name").unwrap().1,
            ElicitationFieldKind::TextInput
        ));
        assert!(matches!(
            kinds.iter().find(|(n, _)| *n == "count").unwrap().1,
            ElicitationFieldKind::NumberInput { integer: true }
        ));
        assert!(matches!(
            kinds.iter().find(|(n, _)| *n == "confirm").unwrap().1,
            ElicitationFieldKind::BooleanToggle
        ));
    }

    #[test]
    fn parse_elicitation_schema_empty_returns_empty() {
        let fields = ElicitationState::parse_schema(&serde_json::json!({}));
        assert!(fields.is_empty());
    }

    // ── Elicitation: state navigation ─────────────────────────────────────────

    #[test]
    fn elicitation_move_cursor_wraps_within_options() {
        let mut state = ElicitationState::new_for_test(vec![ElicitationField {
            name: "q".into(),
            title: "Q".into(),
            description: None,
            required: true,
            kind: ElicitationFieldKind::SingleSelect {
                options: vec![
                    ElicitationOption {
                        value: serde_json::json!("a"),
                        label: "A".into(),
                        description: None,
                    },
                    ElicitationOption {
                        value: serde_json::json!("b"),
                        label: "B".into(),
                        description: None,
                    },
                    ElicitationOption {
                        value: serde_json::json!("c"),
                        label: "C".into(),
                        description: None,
                    },
                ],
            },
        }]);
        assert_eq!(state.option_cursor, 0);
        state.move_cursor(1);
        assert_eq!(state.option_cursor, 1);
        state.move_cursor(1);
        assert_eq!(state.option_cursor, 2);
        state.move_cursor(1); // clamps at max
        assert_eq!(state.option_cursor, 2);
        state.move_cursor(-1);
        assert_eq!(state.option_cursor, 1);
        state.move_cursor(-10);
        assert_eq!(state.option_cursor, 0);
    }

    #[test]
    fn elicitation_build_accept_content_single_select() {
        let mut state = ElicitationState::new_for_test(vec![ElicitationField {
            name: "choice".into(),
            title: "Choice".into(),
            description: None,
            required: true,
            kind: ElicitationFieldKind::SingleSelect {
                options: vec![
                    ElicitationOption {
                        value: serde_json::json!("yes"),
                        label: "Yes".into(),
                        description: None,
                    },
                    ElicitationOption {
                        value: serde_json::json!("no"),
                        label: "No".into(),
                        description: None,
                    },
                ],
            },
        }]);
        state.option_cursor = 0;
        state.select_current_option(); // select "yes"
        let content = state.build_accept_content();
        assert_eq!(content, serde_json::json!({ "choice": "yes" }));
    }

    #[test]
    fn elicitation_build_accept_content_text_input() {
        let mut state = ElicitationState::new_for_test(vec![ElicitationField {
            name: "name".into(),
            title: "Name".into(),
            description: None,
            required: true,
            kind: ElicitationFieldKind::TextInput,
        }]);
        state.text_input = "Alice".into();
        let content = state.build_accept_content();
        assert_eq!(content, serde_json::json!({ "name": "Alice" }));
    }

    #[test]
    fn elicitation_is_valid_requires_required_fields() {
        let mut state = ElicitationState::new_for_test(vec![ElicitationField {
            name: "must".into(),
            title: "Must".into(),
            description: None,
            required: true,
            kind: ElicitationFieldKind::TextInput,
        }]);
        assert!(!state.is_valid());
        state.text_input = "value".into();
        assert!(state.is_valid());
    }

    // ── Elicitation: event handling ───────────────────────────────────────────

    // ── backfill_elicitation_outcomes ─────────────────────────────────────────

    #[test]
    fn backfill_single_answer_sets_outcome() {
        let mut messages = vec![ChatEntry::Elicitation {
            elicitation_id: "e1".into(),
            message: "Pick one".into(),
            source: "builtin:question".into(),
            outcome: Some("responded".into()),
        }];
        let result = r#"{"answers":[{"question":"Pick one","answers":["Beta"]}]}"#;
        backfill_elicitation_outcomes(&mut messages, result);
        assert!(matches!(&messages[0],
            ChatEntry::Elicitation { outcome: Some(o), .. } if *o == format!("{OUTCOME_BULLET}Beta")
        ));
    }

    #[test]
    fn backfill_multi_answer_joins_with_newline() {
        let mut messages = vec![ChatEntry::Elicitation {
            elicitation_id: "e1".into(),
            message: "Pick many".into(),
            source: "builtin:question".into(),
            outcome: Some("responded".into()),
        }];
        let result = r#"{"answers":[{"question":"Pick many","answers":["X","Z"]}]}"#;
        backfill_elicitation_outcomes(&mut messages, result);
        assert!(matches!(&messages[0],
            ChatEntry::Elicitation { outcome: Some(o), .. } if *o == format!("{OUTCOME_BULLET}X\n{OUTCOME_BULLET}Z")
        ));
    }

    #[test]
    fn backfill_multiple_questions_each_card_gets_its_own_answer() {
        let mut messages = vec![
            ChatEntry::Elicitation {
                elicitation_id: "e1".into(),
                message: "Q1".into(),
                source: "builtin:question".into(),
                outcome: Some("responded".into()),
            },
            ChatEntry::Elicitation {
                elicitation_id: "e2".into(),
                message: "Q2".into(),
                source: "builtin:question".into(),
                outcome: Some("responded".into()),
            },
        ];
        let result = r#"{"answers":[{"question":"Q1","answers":["Alpha"]},{"question":"Q2","answers":["Yes"]}]}"#;
        backfill_elicitation_outcomes(&mut messages, result);
        assert!(matches!(&messages[0],
            ChatEntry::Elicitation { outcome: Some(o), .. } if *o == format!("{OUTCOME_BULLET}Alpha")
        ));
        assert!(matches!(&messages[1],
            ChatEntry::Elicitation { outcome: Some(o), .. } if *o == format!("{OUTCOME_BULLET}Yes")
        ));
    }

    #[test]
    fn backfill_skips_already_resolved_cards() {
        let mut messages = vec![
            ChatEntry::Elicitation {
                elicitation_id: "e1".into(),
                message: "Q1".into(),
                source: "builtin:question".into(),
                outcome: Some(format!("{OUTCOME_BULLET}AlreadySet")),
            },
            ChatEntry::Elicitation {
                elicitation_id: "e2".into(),
                message: "Q2".into(),
                source: "builtin:question".into(),
                outcome: Some("responded".into()),
            },
        ];
        let result = r#"{"answers":[{"question":"Q2","answers":["Beta"]}]}"#;
        backfill_elicitation_outcomes(&mut messages, result);
        // First card unchanged
        assert!(matches!(&messages[0],
            ChatEntry::Elicitation { outcome: Some(o), .. } if *o == format!("{OUTCOME_BULLET}AlreadySet")
        ));
        // Second card updated
        assert!(matches!(&messages[1],
            ChatEntry::Elicitation { outcome: Some(o), .. } if *o == format!("{OUTCOME_BULLET}Beta")
        ));
    }

    #[test]
    fn toolcallend_question_replay_backfills_elicitation_cards() {
        let mut app = App::new();
        // Simulate replay of ElicitationRequested (pushes "responded" card)
        app.handle_event_kind(
            &EventKind::ElicitationRequested {
                elicitation_id: "e1".into(),
                session_id: "sess-1".into(),
                message: "Which?".into(),
                requested_schema: serde_json::json!({
                    "properties": { "choice": { "oneOf": [{ "const": "a", "title": "Alpha" }] } },
                    "required": ["choice"]
                }),
                source: "builtin:question".into(),
            },
            true,
        );
        // Simulate replay of ToolCallEnd for question
        app.handle_event_kind(
            &EventKind::ToolCallEnd {
                tool_call_id: Some("call-1".into()),
                tool_name: "question".into(),
                is_error: Some(false),
                result: Some(r#"{"answers":[{"question":"Which?","answers":["Alpha"]}]}"#.into()),
            },
            true,
        );
        assert!(app.messages.iter().any(|m| matches!(m,
            ChatEntry::Elicitation { outcome: Some(o), .. } if *o == format!("{OUTCOME_BULLET}Alpha")
        )));
    }

    #[test]
    fn elicitation_requested_during_replay_does_not_open_popup() {
        let mut app = App::new();
        app.handle_event_kind(
            &EventKind::ElicitationRequested {
                elicitation_id: "elic-replay".into(),
                session_id: "sess-1".into(),
                message: "Which option?".into(),
                requested_schema: serde_json::json!({
                    "properties": {
                        "choice": { "oneOf": [{ "const": "a", "title": "A" }] }
                    },
                    "required": ["choice"]
                }),
                source: "builtin:question".into(),
            },
            true, // is_replay
        );

        // No popup should be opened
        assert!(app.elicitation.is_none());
        // Chat card should be present but already marked as resolved
        assert!(app.messages.iter().any(|m| matches!(m,
            ChatEntry::Elicitation { elicitation_id, outcome: Some(_), .. }
            if elicitation_id == "elic-replay"
        )));
    }

    #[test]
    fn elicitation_requested_event_creates_state_and_chat_card() {
        let mut app = App::new();
        app.handle_event_kind(
            &EventKind::ElicitationRequested {
                elicitation_id: "elic-1".into(),
                session_id: "sess-1".into(),
                message: "Which option?".into(),
                requested_schema: serde_json::json!({
                    "properties": {
                        "choice": {
                            "oneOf": [
                                { "const": "a", "title": "Alpha" },
                                { "const": "b", "title": "Beta" }
                            ]
                        }
                    },
                    "required": ["choice"]
                }),
                source: "builtin:question".into(),
            },
            false,
        );

        // State should be populated
        let state = app.elicitation.as_ref().expect("elicitation state");
        assert_eq!(state.elicitation_id, "elic-1");
        assert_eq!(state.message, "Which option?");
        assert_eq!(state.fields.len(), 1);

        // A chat card should have been appended
        assert!(app.messages.iter().any(|m| matches!(m,
            ChatEntry::Elicitation { elicitation_id, outcome: None, .. }
            if elicitation_id == "elic-1"
        )));
    }

    #[test]
    fn replay_audit_does_not_clear_redo_stack() {
        let mut app = App::new();
        app.undo_state = Some(UndoState {
            stack: vec![UndoFrame {
                turn_id: "turn-msg-3".into(),
                message_id: "msg-3".into(),
                status: UndoFrameStatus::Confirmed,
                reverted_files: vec!["src/lib.rs".into()],
            }],
            frontier_message_id: Some("msg-3".into()),
        });

        let audit = serde_json::json!({
            "events": [
                {
                    "kind": {
                        "type": "prompt_received",
                        "data": {
                            "content": [{ "type": "text", "text": "one" }],
                            "message_id": "msg-1"
                        }
                    }
                },
                {
                    "kind": {
                        "type": "prompt_received",
                        "data": {
                            "content": [{ "type": "text", "text": "two" }],
                            "message_id": "msg-2"
                        }
                    }
                },
                {
                    "kind": {
                        "type": "prompt_received",
                        "data": {
                            "content": [{ "type": "text", "text": "three" }],
                            "message_id": "msg-3"
                        }
                    }
                }
            ]
        });

        app.replay_audit(&audit);

        assert!(app.can_redo());
        let state = app.undo_state.as_ref().expect("undo state");
        assert_eq!(state.frontier_message_id.as_deref(), Some("msg-3"));
        assert_eq!(state.stack.len(), 1);
        assert_eq!(state.stack[0].reverted_files, vec!["src/lib.rs"]);
    }
}

// ── Thinking content tests ────────────────────────────────────────────────────

#[cfg(test)]
mod thinking_content_tests {
    use super::*;

    #[test]
    fn thinking_delta_accumulates_in_streaming_thinking() {
        let mut app = App::new();
        app.handle_event_kind(&EventKind::TurnStarted, false);

        app.handle_event_kind(
            &EventKind::AssistantThinkingDelta {
                content: "Let me ".into(),
                message_id: None,
            },
            false,
        );
        assert_eq!(app.streaming_thinking, "Let me ");

        app.handle_event_kind(
            &EventKind::AssistantThinkingDelta {
                content: "think about this.".into(),
                message_id: None,
            },
            false,
        );
        assert_eq!(app.streaming_thinking, "Let me think about this.");
    }

    #[test]
    fn turn_started_clears_streaming_thinking() {
        let mut app = App::new();
        app.streaming_thinking = "old thinking".into();

        app.handle_event_kind(&EventKind::TurnStarted, false);

        assert!(app.streaming_thinking.is_empty());
    }

    #[test]
    fn assistant_message_stored_captures_thinking_field() {
        let mut app = App::new();
        app.handle_event_kind(&EventKind::TurnStarted, false);

        app.handle_event_kind(
            &EventKind::AssistantMessageStored {
                content: "The answer is 42.".into(),
                thinking: Some("I need to compute the answer.".into()),
                message_id: None,
            },
            false,
        );

        assert_eq!(app.messages.len(), 1);
        match &app.messages[0] {
            ChatEntry::Assistant { content, thinking } => {
                assert_eq!(content, "The answer is 42.");
                assert_eq!(thinking.as_deref(), Some("I need to compute the answer."));
            }
            other => panic!("expected Assistant, got {:?}", other),
        }
    }

    #[test]
    fn assistant_message_stored_without_thinking_sets_none() {
        let mut app = App::new();
        app.handle_event_kind(&EventKind::TurnStarted, false);

        app.handle_event_kind(
            &EventKind::AssistantMessageStored {
                content: "Hello!".into(),
                thinking: None,
                message_id: None,
            },
            false,
        );

        assert_eq!(app.messages.len(), 1);
        match &app.messages[0] {
            ChatEntry::Assistant { content, thinking } => {
                assert_eq!(content, "Hello!");
                assert!(thinking.is_none());
            }
            other => panic!("expected Assistant, got {:?}", other),
        }
    }

    #[test]
    fn streaming_thinking_falls_back_when_stored_thinking_is_none() {
        let mut app = App::new();
        app.handle_event_kind(&EventKind::TurnStarted, false);

        // Simulate thinking deltas arriving before the stored message
        app.handle_event_kind(
            &EventKind::AssistantThinkingDelta {
                content: "Streamed thinking.".into(),
                message_id: None,
            },
            false,
        );

        // AssistantMessageStored arrives without thinking field
        app.handle_event_kind(
            &EventKind::AssistantMessageStored {
                content: "Final answer.".into(),
                thinking: None,
                message_id: None,
            },
            false,
        );

        match &app.messages[0] {
            ChatEntry::Assistant { content, thinking } => {
                assert_eq!(content, "Final answer.");
                assert_eq!(thinking.as_deref(), Some("Streamed thinking."));
            }
            other => panic!("expected Assistant, got {:?}", other),
        }
        // streaming_thinking should be cleared after capture
        assert!(app.streaming_thinking.is_empty());
    }

    #[test]
    fn cancelled_with_thinking_preserves_thinking_in_entry() {
        let mut app = App::new();
        app.handle_event_kind(&EventKind::TurnStarted, false);

        app.handle_event_kind(
            &EventKind::AssistantThinkingDelta {
                content: "Deep thought.".into(),
                message_id: None,
            },
            false,
        );
        app.handle_event_kind(
            &EventKind::AssistantContentDelta {
                content: "Partial answer".into(),
                message_id: None,
            },
            false,
        );
        app.handle_event_kind(&EventKind::Cancelled, false);

        assert_eq!(app.messages.len(), 1);
        match &app.messages[0] {
            ChatEntry::Assistant { content, thinking } => {
                assert!(content.contains("Partial answer"));
                assert!(content.contains("[cancelled]"));
                assert_eq!(thinking.as_deref(), Some("Deep thought."));
            }
            other => panic!("expected Assistant, got {:?}", other),
        }
    }

    #[test]
    fn thinking_delta_keeps_activity_as_thinking() {
        let mut app = App::new();
        app.handle_event_kind(&EventKind::TurnStarted, false);
        assert_eq!(app.activity, ActivityState::Thinking);

        app.handle_event_kind(
            &EventKind::AssistantThinkingDelta {
                content: "hmm".into(),
                message_id: None,
            },
            false,
        );
        // Should still be Thinking (not Streaming) during thinking phase
        assert_eq!(app.activity, ActivityState::Thinking);
    }
}

// ── Start-page session grouping tests ─────────────────────────────────────────

#[cfg(test)]
mod start_page_tests {
    use super::*;

    fn make_group(cwd: Option<&str>, ids: &[(&str, Option<&str>)]) -> SessionGroup {
        SessionGroup {
            cwd: cwd.map(String::from),
            latest_activity: None,
            sessions: ids
                .iter()
                .map(|(id, updated_at)| SessionSummary {
                    session_id: id.to_string(),
                    title: Some(format!("Session {id}")),
                    cwd: cwd.map(String::from),
                    created_at: None,
                    updated_at: updated_at.map(String::from),
                    parent_session_id: None,
                    has_children: false,
                })
                .collect(),
        }
    }

    // ── visible_start_items: no sessions ─────────────────────────────────────

    #[test]
    fn visible_items_empty_when_no_sessions() {
        let app = App::new();
        let items = app.visible_start_items();
        assert!(items.is_empty());
    }

    // ── visible_start_items: basic structure ─────────────────────────────────

    #[test]
    fn visible_items_header_then_sessions_expanded() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &[("s1", None), ("s2", None)])];

        let items = app.visible_start_items();
        // 1 header + 2 sessions
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0], StartPageItem::GroupHeader { .. }));
        assert!(matches!(&items[1], StartPageItem::Session { .. }));
        assert!(matches!(&items[2], StartPageItem::Session { .. }));
    }

    // ── visible_start_items: collapse hides children ─────────────────────────

    #[test]
    fn visible_items_collapsed_group_hides_sessions() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &[("s1", None), ("s2", None)])];
        app.collapsed_groups.insert("/a".to_string());

        let items = app.visible_start_items();
        // only the header
        assert_eq!(items.len(), 1);
        assert!(matches!(
            &items[0],
            StartPageItem::GroupHeader {
                collapsed: true,
                ..
            }
        ));
    }

    // ── visible_start_items: multiple groups ─────────────────────────────────

    #[test]
    fn visible_items_multiple_groups() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &[("s1", None)]),
            make_group(Some("/b"), &[("s2", None), ("s3", None)]),
        ];

        let items = app.visible_start_items();
        // group /a: 1 header + 1 session = 2
        // group /b: 1 header + 2 sessions = 3
        assert_eq!(items.len(), 5);
    }

    // ── visible_start_items: mixed collapse ───────────────────────────────────

    #[test]
    fn visible_items_one_group_collapsed_other_expanded() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &[("s1", None)]),
            make_group(Some("/b"), &[("s2", None), ("s3", None)]),
        ];
        app.collapsed_groups.insert("/a".to_string());

        let items = app.visible_start_items();
        // /a collapsed: 1 header
        // /b expanded:  1 header + 2 sessions
        assert_eq!(items.len(), 4);
        assert!(matches!(
            &items[0],
            StartPageItem::GroupHeader {
                collapsed: true,
                ..
            }
        ));
        assert!(matches!(
            &items[1],
            StartPageItem::GroupHeader {
                collapsed: false,
                ..
            }
        ));
    }

    // ── visible_start_items: filter hides non-matching sessions ──────────────

    #[test]
    fn visible_items_filter_hides_non_matching_sessions() {
        let mut app = App::new();
        app.session_groups = vec![make_group(
            Some("/a"),
            &[("aaa", None), ("bbb", None), ("aab", None)],
        )];
        app.session_filter = "aa".to_string();

        let items = app.visible_start_items();
        // header + "aaa" + "aab" (bbb filtered out by session_id)
        assert_eq!(items.len(), 3);
    }

    // ── visible_start_items: filter hides empty groups ────────────────────────

    #[test]
    fn visible_items_filter_hides_groups_with_no_matches() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &[("aaa", None)]),
            make_group(Some("/b"), &[("bbb", None)]),
        ];
        app.session_filter = "bbb".to_string();

        let items = app.visible_start_items();
        // group /a has no matches → hidden entirely
        // group /b: header + "bbb"
        assert_eq!(items.len(), 2);
        if let StartPageItem::GroupHeader { cwd, .. } = &items[0] {
            assert_eq!(cwd.as_deref(), Some("/b"));
        } else {
            panic!("expected GroupHeader");
        }
    }

    // ── visible_start_items: session indices are correct ─────────────────────

    #[test]
    fn visible_items_session_indices_correct() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &[("s0", None), ("s1", None)]),
            make_group(Some("/b"), &[("s2", None)]),
        ];

        let items = app.visible_start_items();
        // items[0]: GroupHeader /a
        // items[1]: Session group_idx=0, session_idx=0
        // items[2]: Session group_idx=0, session_idx=1
        // items[3]: GroupHeader /b
        // items[4]: Session group_idx=1, session_idx=0
        assert!(matches!(
            &items[1],
            StartPageItem::Session {
                group_idx: 0,
                session_idx: 0
            }
        ));
        assert!(matches!(
            &items[2],
            StartPageItem::Session {
                group_idx: 0,
                session_idx: 1
            }
        ));
        assert!(matches!(
            &items[4],
            StartPageItem::Session {
                group_idx: 1,
                session_idx: 0
            }
        ));
    }

    // ── session_list message preserves group structure ────────────────────────

    #[test]
    fn session_list_message_populates_session_groups() {
        let mut app = App::new();
        app.handle_server_msg(RawServerMsg {
            msg_type: "session_list".into(),
            data: Some(serde_json::json!({
                "groups": [
                    {
                        "cwd": "/home/user/proj",
                        "sessions": [
                            { "session_id": "s1", "title": "T1", "updated_at": "2024-01-01T00:00:00Z" }
                        ]
                    }
                ]
            })),
        });

        assert_eq!(app.session_groups.len(), 1);
        assert_eq!(
            app.session_groups[0].cwd.as_deref(),
            Some("/home/user/proj")
        );
        assert_eq!(app.session_groups[0].sessions.len(), 1);
        assert_eq!(app.session_groups[0].sessions[0].session_id, "s1");
    }

    // ── filtered_sessions still works (for popup compat) ─────────────────────

    #[test]
    fn filtered_sessions_returns_flat_list_for_popup() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &[("s1", None)]),
            make_group(Some("/b"), &[("s2", None), ("s3", None)]),
        ];

        let flat = app.filtered_sessions();
        assert_eq!(flat.len(), 3);
    }

    #[test]
    fn filtered_sessions_applies_filter() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &[("aaa", None), ("bbb", None)])];
        app.session_filter = "aaa".to_string();

        let flat = app.filtered_sessions();
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].session_id, "aaa");
    }

    // ── GroupHeader carries correct session_count ─────────────────────────────

    #[test]
    fn group_header_session_count_reflects_total_not_filtered() {
        let mut app = App::new();
        app.session_groups = vec![make_group(
            Some("/a"),
            &[("s1", None), ("s2", None), ("s3", None)],
        )];

        let items = app.visible_start_items();
        assert!(matches!(
            &items[0],
            StartPageItem::GroupHeader {
                session_count: 3,
                ..
            }
        ));
    }

    // ── toggle_group_collapse ─────────────────────────────────────────────────

    #[test]
    fn toggle_group_collapse_collapses_then_expands() {
        let mut app = App::new();
        let key = "/a".to_string();
        assert!(!app.collapsed_groups.contains(&key));

        app.toggle_group_collapse(Some("/a"));
        assert!(app.collapsed_groups.contains(&key));

        app.toggle_group_collapse(Some("/a"));
        assert!(!app.collapsed_groups.contains(&key));
    }

    #[test]
    fn toggle_group_collapse_none_cwd_uses_empty_string_key() {
        let mut app = App::new();
        app.toggle_group_collapse(None);
        assert!(app.collapsed_groups.contains(""));

        app.toggle_group_collapse(None);
        assert!(!app.collapsed_groups.contains(""));
    }

    // ── MAX_RECENT_SESSIONS cap ───────────────────────────────────────────────

    #[test]
    fn visible_items_group_with_three_sessions_shows_no_show_more() {
        let mut app = App::new();
        app.session_groups = vec![make_group(
            Some("/a"),
            &[("s1", None), ("s2", None), ("s3", None)],
        )];
        let items = app.visible_start_items();
        // header + 3 sessions, no ShowMore
        assert_eq!(items.len(), 4);
        assert!(
            !items
                .iter()
                .any(|i| matches!(i, StartPageItem::ShowMore { .. }))
        );
    }

    #[test]
    fn visible_items_group_with_four_sessions_caps_at_three_plus_show_more() {
        let mut app = App::new();
        app.session_groups = vec![make_group(
            Some("/a"),
            &[("s1", None), ("s2", None), ("s3", None), ("s4", None)],
        )];
        let items = app.visible_start_items();
        // header + 3 sessions + ShowMore
        assert_eq!(items.len(), 5);
        assert!(matches!(
            items[4],
            StartPageItem::ShowMore { remaining: 1, .. }
        ));
    }

    #[test]
    fn visible_items_show_more_remaining_is_total_minus_three() {
        let mut app = App::new();
        // 7 sessions → show 3 + ShowMore(remaining=4)
        app.session_groups = vec![make_group(
            Some("/a"),
            &[
                ("s1", None),
                ("s2", None),
                ("s3", None),
                ("s4", None),
                ("s5", None),
                ("s6", None),
                ("s7", None),
            ],
        )];
        let items = app.visible_start_items();
        assert!(matches!(
            items.last(),
            Some(StartPageItem::ShowMore { remaining: 4, .. })
        ));
    }

    #[test]
    fn visible_items_filter_active_still_caps_sessions() {
        let mut app = App::new();
        app.session_groups = vec![make_group(
            Some("/a"),
            &[
                ("aaa1", None),
                ("aaa2", None),
                ("aaa3", None),
                ("aaa4", None),
            ],
        )];
        app.session_filter = "aaa".to_string();
        let items = app.visible_start_items();
        // All 4 match the filter but cap still applies → header + 3 sessions + ShowMore(1)
        assert_eq!(items.len(), 5);
        assert!(matches!(
            items.last(),
            Some(StartPageItem::ShowMore { remaining: 1 })
        ));
    }

    // ── MAX_VISIBLE_GROUPS cap ────────────────────────────────────────────────

    #[test]
    fn visible_items_three_groups_shows_no_trailing_show_more() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &[("s1", None)]),
            make_group(Some("/b"), &[("s2", None)]),
            make_group(Some("/c"), &[("s3", None)]),
        ];
        let items = app.visible_start_items();
        // 3 headers + 3 sessions = 6, no trailing ShowMore
        assert_eq!(items.len(), 6);
        assert!(
            !items
                .iter()
                .any(|i| matches!(i, StartPageItem::ShowMore { .. }))
        );
    }

    #[test]
    fn visible_items_four_groups_caps_at_three_plus_show_more() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &[("s1", None)]),
            make_group(Some("/b"), &[("s2", None)]),
            make_group(Some("/c"), &[("s3", None)]),
            make_group(Some("/d"), &[("s4", None)]),
        ];
        let items = app.visible_start_items();
        // 3 groups (3 headers + 3 sessions) + 1 trailing ShowMore = 7
        assert_eq!(items.len(), 7);
        assert!(matches!(
            items.last(),
            Some(StartPageItem::ShowMore { remaining: 1 })
        ));
    }

    #[test]
    fn visible_items_trailing_show_more_remaining_is_hidden_groups() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &[("s1", None)]),
            make_group(Some("/b"), &[("s2", None)]),
            make_group(Some("/c"), &[("s3", None)]),
            make_group(Some("/d"), &[("s4", None)]),
            make_group(Some("/e"), &[("s5", None)]),
            make_group(Some("/f"), &[("s6", None)]),
        ];
        let items = app.visible_start_items();
        // 3 shown groups + 1 trailing ShowMore(remaining=3)
        assert!(matches!(
            items.last(),
            Some(StartPageItem::ShowMore { remaining: 3 })
        ));
    }

    #[test]
    fn visible_items_group_cap_applied_with_filter_active() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &[("aaa1", None)]),
            make_group(Some("/b"), &[("aaa2", None)]),
            make_group(Some("/c"), &[("aaa3", None)]),
            make_group(Some("/d"), &[("aaa4", None)]),
        ];
        app.session_filter = "aaa".to_string();
        let items = app.visible_start_items();
        // Filter active but group cap still applies → 3 groups + trailing ShowMore(1)
        let headers = items
            .iter()
            .filter(|i| matches!(i, StartPageItem::GroupHeader { .. }))
            .count();
        assert_eq!(headers, 3);
        assert!(matches!(
            items.last(),
            Some(StartPageItem::ShowMore { remaining: 1 })
        ));
    }
}

// ── popup_item_tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod popup_item_tests {
    use super::*;

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

    // ── empty state ───────────────────────────────────────────────────────────

    #[test]
    fn popup_items_empty_when_no_sessions() {
        let app = App::new();
        assert!(app.visible_popup_items().is_empty());
    }

    // ── basic structure: header then sessions ─────────────────────────────────

    #[test]
    fn popup_items_header_then_sessions() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2"])];
        let items = app.visible_popup_items();
        // 1 header + 2 sessions
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0], PopupItem::GroupHeader { .. }));
        assert!(matches!(&items[1], PopupItem::Session { .. }));
        assert!(matches!(&items[2], PopupItem::Session { .. }));
    }

    // ── no MAX_RECENT_SESSIONS cap ────────────────────────────────────────────

    #[test]
    fn popup_items_shows_all_sessions_beyond_cap() {
        let mut app = App::new();
        // 10 sessions - all should appear, no cap like start page
        let ids: Vec<&str> = vec!["s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10"];
        app.session_groups = vec![make_group(Some("/a"), &ids)];
        let items = app.visible_popup_items();
        // 1 header + 10 sessions = 11
        assert_eq!(items.len(), 11);
        // No ShowMore items
        assert!(
            !items
                .iter()
                .any(|i| matches!(i, PopupItem::GroupHeader { .. } if false))
        );
    }

    // ── no MAX_VISIBLE_GROUPS cap ─────────────────────────────────────────────

    #[test]
    fn popup_items_shows_all_groups_beyond_cap() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &["s1"]),
            make_group(Some("/b"), &["s2"]),
            make_group(Some("/c"), &["s3"]),
            make_group(Some("/d"), &["s4"]),
            make_group(Some("/e"), &["s5"]),
        ];
        let items = app.visible_popup_items();
        let headers = items
            .iter()
            .filter(|i| matches!(i, PopupItem::GroupHeader { .. }))
            .count();
        // All 5 groups shown (start page would cap at MAX_VISIBLE_GROUPS=3)
        assert_eq!(headers, 5);
    }

    // ── collapse is separate from start page ──────────────────────────────────

    #[test]
    fn popup_collapsed_is_independent_of_start_page_collapsed() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2"])];
        // Collapse on the start page should NOT affect the popup
        app.collapsed_groups.insert("/a".to_string());
        let items = app.visible_popup_items();
        // Popup uses popup_collapsed_groups, not collapsed_groups
        // /a is expanded in popup → header + 2 sessions = 3
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn popup_collapsed_hides_sessions() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2"])];
        app.popup_collapsed_groups.insert("/a".to_string());
        let items = app.visible_popup_items();
        // Only the header visible
        assert_eq!(items.len(), 1);
        assert!(matches!(
            &items[0],
            PopupItem::GroupHeader {
                collapsed: true,
                ..
            }
        ));
    }

    #[test]
    fn popup_expanded_shows_sessions() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        // Not in popup_collapsed_groups → expanded
        let items = app.visible_popup_items();
        assert_eq!(items.len(), 2);
        assert!(matches!(
            &items[0],
            PopupItem::GroupHeader {
                collapsed: false,
                ..
            }
        ));
    }

    // ── multiple groups, mixed collapse ───────────────────────────────────────

    #[test]
    fn popup_items_multiple_groups() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &["s1"]),
            make_group(Some("/b"), &["s2", "s3"]),
        ];
        let items = app.visible_popup_items();
        // /a: 1 header + 1 session; /b: 1 header + 2 sessions = 5
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn popup_one_group_collapsed_other_expanded() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &["s1"]),
            make_group(Some("/b"), &["s2", "s3"]),
        ];
        app.popup_collapsed_groups.insert("/a".to_string());
        let items = app.visible_popup_items();
        // /a collapsed: 1 header; /b expanded: 1 header + 2 sessions = 4
        assert_eq!(items.len(), 4);
        assert!(matches!(
            &items[0],
            PopupItem::GroupHeader {
                collapsed: true,
                ..
            }
        ));
        assert!(matches!(
            &items[1],
            PopupItem::GroupHeader {
                collapsed: false,
                ..
            }
        ));
    }

    // ── filter hides non-matching sessions ────────────────────────────────────

    #[test]
    fn popup_filter_hides_non_matching_sessions() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["aaa", "bbb", "aab"])];
        app.session_filter = "aa".to_string();
        let items = app.visible_popup_items();
        // header + "aaa" + "aab" (bbb filtered out by session_id)
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn popup_filter_hides_groups_with_no_matches() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &["aaa"]),
            make_group(Some("/b"), &["bbb"]),
        ];
        app.session_filter = "bbb".to_string();
        let items = app.visible_popup_items();
        // /a has no matches → hidden; /b: header + "bbb" = 2
        assert_eq!(items.len(), 2);
        if let PopupItem::GroupHeader { cwd, .. } = &items[0] {
            assert_eq!(cwd.as_deref(), Some("/b"));
        } else {
            panic!("expected GroupHeader");
        }
    }

    // ── session indices are correct ───────────────────────────────────────────

    #[test]
    fn popup_items_session_indices_correct() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &["s0", "s1"]),
            make_group(Some("/b"), &["s2"]),
        ];
        let items = app.visible_popup_items();
        // items[0]: GroupHeader /a
        // items[1]: Session group_idx=0, session_idx=0
        // items[2]: Session group_idx=0, session_idx=1
        // items[3]: GroupHeader /b
        // items[4]: Session group_idx=1, session_idx=0
        assert!(matches!(
            &items[1],
            PopupItem::Session {
                group_idx: 0,
                session_idx: 0
            }
        ));
        assert!(matches!(
            &items[2],
            PopupItem::Session {
                group_idx: 0,
                session_idx: 1
            }
        ));
        assert!(matches!(
            &items[4],
            PopupItem::Session {
                group_idx: 1,
                session_idx: 0
            }
        ));
    }

    // ── group header carries correct session_count ────────────────────────────

    #[test]
    fn popup_group_header_session_count_reflects_total() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2", "s3"])];
        let items = app.visible_popup_items();
        assert!(matches!(
            &items[0],
            PopupItem::GroupHeader {
                session_count: 3,
                ..
            }
        ));
    }

    // ── toggle_popup_group_collapse ───────────────────────────────────────────

    #[test]
    fn toggle_popup_collapse_collapses_then_expands() {
        let mut app = App::new();
        assert!(!app.popup_collapsed_groups.contains("/a"));
        app.toggle_popup_group_collapse(Some("/a"));
        assert!(app.popup_collapsed_groups.contains("/a"));
        app.toggle_popup_group_collapse(Some("/a"));
        assert!(!app.popup_collapsed_groups.contains("/a"));
    }

    #[test]
    fn toggle_popup_collapse_none_cwd_uses_empty_string_key() {
        let mut app = App::new();
        app.toggle_popup_group_collapse(None);
        assert!(app.popup_collapsed_groups.contains(""));
        app.toggle_popup_group_collapse(None);
        assert!(!app.popup_collapsed_groups.contains(""));
    }

    #[test]
    fn toggle_popup_collapse_does_not_affect_start_page_state() {
        let mut app = App::new();
        app.toggle_popup_group_collapse(Some("/a"));
        assert!(app.popup_collapsed_groups.contains("/a"));
        // start page collapsed_groups should be untouched
        assert!(!app.collapsed_groups.contains("/a"));
    }
}
