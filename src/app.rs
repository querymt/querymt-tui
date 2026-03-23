use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use ratatui::text::Line;

use crate::highlight::Highlighter;
use crate::protocol::*;
use crate::ui::{CardCache, ELLIPSIS, OUTCOME_BULLET, build_diff_lines, build_write_lines};

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
}

#[derive(Debug, Clone)]
pub enum ChatEntry {
    User {
        text: String,
        message_id: Option<String>,
    },
    Assistant(String),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveCompactionState {
    pub token_estimate: u32,
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
        if let ElicitationFieldKind::SingleSelect { options } = &field.kind {
            if let Some(opt) = options.get(self.option_cursor) {
                let name = field.name.clone();
                let value = opt.value.clone();
                self.selected.insert(name, value);
            }
        }
    }

    /// For MultiSelect: toggle the highlighted option in the field's array value.
    pub fn toggle_current_option(&mut self) {
        let field = self.current_field();
        if let ElicitationFieldKind::MultiSelect { options } = &field.kind {
            if let Some(opt) = options.get(self.option_cursor) {
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
    pub scroll_offset: u16,
    pub is_thinking: bool,
    pub pending_session_op: Option<SessionOp>,
    pub streaming_content: String,
    pub streaming_cache: StreamingCache,
    pub file_index: Vec<FileIndexEntryLite>,
    pub file_index_generated_at: Option<u64>,
    pub file_index_loading: bool,
    pub file_index_error: Option<String>,
    pub mention_state: Option<MentionState>,
    pub live_compaction: Option<LiveCompactionState>,
    pub last_compaction_token_estimate: Option<u32>,
    /// Active elicitation request waiting for user response.
    pub elicitation: Option<ElicitationState>,

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
            scroll_offset: 0,
            is_thinking: false,
            pending_session_op: None,
            streaming_content: String::new(),
            streaming_cache: StreamingCache::new(),
            file_index: Vec::new(),
            file_index_generated_at: None,
            file_index_loading: false,
            file_index_error: None,
            mention_state: None,
            live_compaction: None,
            last_compaction_token_estimate: None,
            elicitation: None,
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
            undo_state: None,
            undoable_turns: Vec::new(),
            cumulative_cost: None,
            context_limit: 0,
            session_stats: SessionStatsLite::default(),
            pending_cancel_confirm_until: None,
            conn: ConnState::Connecting,
            reconnect_attempt: 0,
            reconnect_delay_ms: None,
            hl: Highlighter::new(),
            card_cache: CardCache::new(),
            status: "connecting...".into(),
            tick: 0,
            should_quit: false,
        }
    }

    /// Flat list of sessions that match the current filter, across all groups.
    ///
    /// Used by the session popup (which shows a flat list) for backward compatibility.
    pub fn filtered_sessions(&self) -> Vec<&SessionSummary> {
        let q = self.session_filter.to_lowercase();
        self.session_groups
            .iter()
            .flat_map(|g| g.sessions.iter())
            .filter(|s| {
                q.is_empty()
                    || s.title.as_deref().unwrap_or("").to_lowercase().contains(&q)
                    || s.session_id.to_lowercase().contains(&q)
            })
            .collect()
    }

    /// Build the flat list of visible rows for the start-page session list.
    ///
    /// Each call re-evaluates the current `session_filter` and `collapsed_groups`.
    /// Group headers are always included; session rows are included only when
    /// the group is expanded *and* the session matches the filter.
    /// Groups with zero matching sessions are omitted entirely when a filter is
    /// active.
    pub fn visible_start_items(&self) -> Vec<StartPageItem> {
        let q = self.session_filter.to_lowercase();
        let mut items = Vec::new();

        // When no filter is active, cap the number of visible groups.
        let cap_groups = q.is_empty();
        let hidden_groups = if cap_groups {
            self.session_groups.len().saturating_sub(MAX_VISIBLE_GROUPS)
        } else {
            0
        };

        let groups_iter: Box<dyn Iterator<Item = (usize, &SessionGroup)>> = if cap_groups {
            Box::new(
                self.session_groups
                    .iter()
                    .enumerate()
                    .take(MAX_VISIBLE_GROUPS),
            )
        } else {
            Box::new(self.session_groups.iter().enumerate())
        };

        for (group_idx, group) in groups_iter {
            let collapse_key = group.cwd.clone().unwrap_or_default();
            let collapsed = self.collapsed_groups.contains(&collapse_key);

            // Determine which session indices survive the filter.
            let matching: Vec<usize> = group
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    q.is_empty()
                        || s.title.as_deref().unwrap_or("").to_lowercase().contains(&q)
                        || s.session_id.to_lowercase().contains(&q)
                })
                .map(|(i, _)| i)
                .collect();

            // When a filter is active, skip groups with no matches entirely.
            if !q.is_empty() && matching.is_empty() {
                continue;
            }

            items.push(StartPageItem::GroupHeader {
                cwd: group.cwd.clone(),
                session_count: group.sessions.len(),
                collapsed,
            });

            if !collapsed {
                // When a filter is active show all matches; otherwise cap at
                // MAX_RECENT_SESSIONS and append a ShowMore row if needed.
                let capped = q.is_empty();
                let visible: Vec<usize> = if capped {
                    matching.iter().copied().take(MAX_RECENT_SESSIONS).collect()
                } else {
                    matching.clone()
                };
                let hidden = if capped {
                    matching.len().saturating_sub(MAX_RECENT_SESSIONS)
                } else {
                    0
                };

                for session_idx in visible {
                    items.push(StartPageItem::Session {
                        group_idx,
                        session_idx,
                    });
                }

                if hidden > 0 {
                    items.push(StartPageItem::ShowMore { remaining: hidden });
                }
            }
        }

        // Trailing ShowMore for hidden groups (only when filter is inactive).
        if hidden_groups > 0 {
            items.push(StartPageItem::ShowMore { remaining: hidden_groups });
        }

        items
    }

    /// Toggle the collapsed state of the group identified by `cwd`.
    ///
    /// `None` cwd is stored under the empty-string key so it can still be
    /// toggled independently.
    pub fn toggle_group_collapse(&mut self, cwd: Option<&str>) {
        let key = cwd.unwrap_or("").to_string();
        if !self.collapsed_groups.remove(&key) {
            self.collapsed_groups.insert(key);
        }
    }

    /// Toggle the collapsed state of a group *in the session popup*.
    ///
    /// Uses `popup_collapsed_groups` — fully independent of the start-page
    /// `collapsed_groups` so the two views never interfere.
    pub fn toggle_popup_group_collapse(&mut self, cwd: Option<&str>) {
        let key = cwd.unwrap_or("").to_string();
        if !self.popup_collapsed_groups.remove(&key) {
            self.popup_collapsed_groups.insert(key);
        }
    }

    /// Build the flat list of visible rows for the session popup.
    ///
    /// Mirrors [`visible_start_items`] but with two key differences:
    /// - Uses `popup_collapsed_groups` instead of `collapsed_groups`.
    /// - No `MAX_RECENT_SESSIONS` or `MAX_VISIBLE_GROUPS` caps — the popup
    ///   always shows every group and every session (its purpose is to browse
    ///   the full list).
    pub fn visible_popup_items(&self) -> Vec<PopupItem> {
        let q = self.session_filter.to_lowercase();
        let mut items = Vec::new();

        for (group_idx, group) in self.session_groups.iter().enumerate() {
            let collapse_key = group.cwd.clone().unwrap_or_default();
            let collapsed = self.popup_collapsed_groups.contains(&collapse_key);

            // Determine which session indices survive the filter.
            let matching: Vec<usize> = group
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    q.is_empty()
                        || s.title.as_deref().unwrap_or("").to_lowercase().contains(&q)
                        || s.session_id.to_lowercase().contains(&q)
                })
                .map(|(i, _)| i)
                .collect();

            // When a filter is active, skip groups with no matches entirely.
            if !q.is_empty() && matching.is_empty() {
                continue;
            }

            items.push(PopupItem::GroupHeader {
                cwd: group.cwd.clone(),
                session_count: group.sessions.len(),
                collapsed,
            });

            if !collapsed {
                for session_idx in matching {
                    items.push(PopupItem::Session {
                        group_idx,
                        session_idx,
                    });
                }
            }
        }

        items
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
        self.session_cache
            .entry(sid)
            .or_default()
            .insert(
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
        let current_model_key = match (self.current_provider.as_deref(), self.current_model.as_deref()) {
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

    pub fn cancel_confirm_active(&self) -> bool {
        self.pending_cancel_confirm_until
            .map(|deadline| Instant::now() <= deadline)
            .unwrap_or(false)
    }

    pub fn arm_cancel_confirm(&mut self) {
        self.pending_cancel_confirm_until = Some(Instant::now() + CANCEL_CONFIRM_TIMEOUT);
        self.status = "press Esc again to stop".into();
    }

    pub fn clear_cancel_confirm(&mut self) {
        self.pending_cancel_confirm_until = None;
    }

    pub fn refresh_transient_status(&mut self) {
        if self.pending_cancel_confirm_until.is_some() {
            return;
        }
        if let Some(op) = self.pending_session_op {
            self.status = match op {
                SessionOp::Undo => "undoing...".into(),
                SessionOp::Redo => "redoing...".into(),
            };
        } else if let Some(compaction) = &self.live_compaction {
            self.status = format!("compacting context (~{} tokens)", compaction.token_estimate);
        } else if self.is_thinking {
            self.status = "thinking...".into();
        } else if self.conn == ConnState::Connected {
            self.status = "ready".into();
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

    pub fn resolve_new_session_default_cwd(&self) -> Option<String> {
        if let Some(active_session_id) = self.session_id.as_deref() {
            for group in &self.session_groups {
                for session in &group.sessions {
                    if session.session_id == active_session_id {
                        if let Some(cwd) = session.cwd.as_ref().filter(|cwd| !cwd.trim().is_empty()) {
                            return Some(cwd.clone());
                        }
                        if let Some(cwd) = group.cwd.as_ref().filter(|cwd| !cwd.trim().is_empty()) {
                            return Some(cwd.clone());
                        }
                    }
                }
            }
        }

        self.launch_cwd
            .as_ref()
            .filter(|cwd| !cwd.trim().is_empty())
            .cloned()
    }

    pub fn open_new_session_popup(&mut self) {
        self.popup = Popup::NewSession;
        self.new_session_path = self.resolve_new_session_default_cwd().unwrap_or_default();
        self.new_session_cursor = self.new_session_path.chars().count();
        self.refresh_new_session_completion();
    }

    pub fn new_session_base_dir(&self) -> PathBuf {
        self.launch_cwd
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn expand_user_path(&self, input: &str) -> PathBuf {
        if input == "~" {
            return dirs::home_dir().unwrap_or_else(|| PathBuf::from(input));
        }
        if let Some(rest) = input.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest);
            }
        }
        PathBuf::from(input)
    }

    fn normalize_lexical_path(&self, path: &Path) -> PathBuf {
        use std::path::Component;

        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
                Component::RootDir => normalized.push(Component::RootDir.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => {
                    if !normalized.pop() {
                        normalized.push(Component::RootDir.as_os_str());
                    }
                }
                Component::Normal(part) => normalized.push(part),
            }
        }
        normalized
    }

    pub fn normalize_new_session_path(&self, input: &str) -> Option<String> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return self.resolve_new_session_default_cwd().map(|cwd| {
                self.normalize_lexical_path(&PathBuf::from(cwd))
                    .to_string_lossy()
                    .into_owned()
            });
        }

        let path = self.expand_user_path(trimmed);
        let absolute = if path.is_absolute() {
            path
        } else {
            self.new_session_base_dir().join(path)
        };
        Some(
            self.normalize_lexical_path(&absolute)
                .to_string_lossy()
                .into_owned(),
        )
    }

    pub fn collect_path_completion_candidates(&self, query: &str) -> Vec<FileIndexEntryLite> {
        let base_dir = self.new_session_base_dir();
        let typed = query.trim();
        let candidate_root = if typed.is_empty() {
            base_dir.clone()
        } else {
            let raw = PathBuf::from(typed);
            if raw.is_absolute() {
                raw.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"))
            } else {
                let joined = base_dir.join(raw);
                joined.parent().map(Path::to_path_buf).unwrap_or(base_dir.clone())
            }
        };

        let Ok(entries) = std::fs::read_dir(&candidate_root) else {
            return Vec::new();
        };

        let mut candidates = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            if !is_dir {
                continue;
            }
            candidates.push(FileIndexEntryLite {
                path: path.to_string_lossy().into_owned(),
                is_dir,
            });
        }
        candidates
    }

    pub fn rank_path_completion_matches(&self, query: &str) -> Vec<FileIndexEntryLite> {
        let matcher = SkimMatcherV2::default();
        let mut scored: Vec<(i64, bool, usize, FileIndexEntryLite)> = self
            .collect_path_completion_candidates(query)
            .into_iter()
            .filter_map(|entry| {
                let path = entry.path.as_str();
                let filename = path.rsplit('/').next().unwrap_or(path);
                let lower_path = path.to_lowercase();
                let lower_filename = filename.to_lowercase();
                let lower_query = query.trim().to_lowercase();

                let mut score = if lower_query.is_empty() {
                    0
                } else {
                    matcher
                        .fuzzy_match(path, query.trim())
                        .or_else(|| matcher.fuzzy_match(filename, query.trim()))?
                };
                if !lower_query.is_empty() && lower_path.starts_with(&lower_query) {
                    score += 10_000;
                }
                if !lower_query.is_empty() && lower_filename.starts_with(&lower_query) {
                    score += 7_500;
                }
                if !lower_query.is_empty() && lower_path.contains(&lower_query) {
                    score += 3_000;
                }

                Some((score, entry.is_dir, path.len(), entry))
            })
            .collect();

        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.3.path.cmp(&b.3.path))
        });

        scored.into_iter().take(6).map(|(_, _, _, entry)| entry).collect()
    }

    pub fn refresh_new_session_completion(&mut self) {
        let query = self.new_session_path.clone();
        let results = self.rank_path_completion_matches(&query);
        self.new_session_completion = Some(PathCompletionState {
            query,
            selected_index: 0,
            results,
        });
    }

    pub fn move_new_session_completion_selection(&mut self, delta: isize) {
        if let Some(completion) = self.new_session_completion.as_mut() {
            let len = completion.results.len();
            if len == 0 {
                completion.selected_index = 0;
                return;
            }
            let next = (completion.selected_index as isize + delta).rem_euclid(len as isize) as usize;
            completion.selected_index = next;
        }
    }

    pub fn accept_selected_new_session_completion(&mut self) -> bool {
        let Some(completion) = self.new_session_completion.clone() else {
            return false;
        };
        let Some(selected) = completion.results.get(completion.selected_index) else {
            return false;
        };

        let mut normalized = self
            .normalize_new_session_path(&selected.path)
            .unwrap_or_else(|| selected.path.clone());
        if selected.is_dir && !normalized.ends_with('/') {
            normalized.push('/');
        }
        self.new_session_path = normalized;
        self.new_session_cursor = self.new_session_path.len();
        self.new_session_completion = None;
        true
    }

    pub fn note_session_activity(&mut self, session_id: &str) {
        self.session_activity.insert(
            session_id.to_string(),
            SessionActivity {
                last_event_at: Instant::now(),
            },
        );
    }

    pub fn active_session_count(&self) -> usize {
        const ACTIVE_SESSION_WINDOW: Duration = Duration::from_secs(5);
        let now = Instant::now();
        self.session_activity
            .values()
            .filter(|activity| now.duration_since(activity.last_event_at) <= ACTIVE_SESSION_WINDOW)
            .count()
    }

    pub fn other_active_session_count(&self) -> usize {
        const ACTIVE_SESSION_WINDOW: Duration = Duration::from_secs(5);
        let now = Instant::now();
        self.session_activity
            .iter()
            .filter(|(session_id, activity)| {
                now.duration_since(activity.last_event_at) <= ACTIVE_SESSION_WINDOW
                    && self.session_id.as_deref() != Some(session_id.as_str())
            })
            .count()
    }

    pub fn handle_connection_event(&mut self, event: ConnectionEvent) {
        self.clear_cancel_confirm();
        match event {
            ConnectionEvent::Connecting { attempt, delay_ms } => {
                self.conn = ConnState::Connecting;
                self.reconnect_attempt = attempt;
                self.reconnect_delay_ms = Some(delay_ms);
                let secs = delay_ms as f64 / 1000.0;
                self.status = format!("waiting for server - retry {attempt} in {secs:.1}s");
            }
            ConnectionEvent::Connected => {
                self.conn = ConnState::Connected;
                self.reconnect_attempt = 0;
                self.reconnect_delay_ms = None;
                self.status = if self.session_id.is_some() {
                    "reconnected".into()
                } else {
                    "connected".into()
                };
            }
            ConnectionEvent::Disconnected { reason } => {
                self.conn = ConnState::Disconnected;
                self.reconnect_delay_ms = None;
                self.status = format!("connection lost - {reason}");
            }
        }
    }

    pub fn handle_server_msg(&mut self, raw: RawServerMsg) -> Vec<ClientMsg> {
        match raw.msg_type.as_str() {
            "state" => {
                if let Some(data) = raw.data {
                    if let Ok(state) = serde_json::from_value::<StateData>(data) {
                        self.agent_id = state.agents.first().map(|a| a.id.clone());
                        if let Some(mode) = state.agent_mode {
                            self.agent_mode = mode;
                        }
                        // Only update reasoning_effort when the key was present in
                        // the JSON; absent means the server didn't report it.
                        match state.reasoning_effort {
                            ReasoningEffortField::Absent => {}
                            ReasoningEffortField::Auto => self.reasoning_effort = None,
                            ReasoningEffortField::Set(s) => self.reasoning_effort = Some(s),
                        }
                        self.conn = ConnState::Connected;
                        self.status = "connected".into();
                    }
                }
                vec![]
            }
            "reasoning_effort" => {
                if let Some(data) = raw.data {
                    if let Ok(re) = serde_json::from_value::<ReasoningEffortData>(data) {
                        self.reasoning_effort = match re.reasoning_effort.as_deref() {
                            None | Some("auto") => None,
                            Some(s) => Some(s.to_string()),
                        };
                        // Server is authoritative — cache so this session + mode
                        // remembers the level across restarts.
                        self.cache_session_mode_state();
                    }
                }
                vec![]
            }
            "agent_mode" => {
                if let Some(data) = raw.data {
                    if let Ok(am) = serde_json::from_value::<AgentModeData>(data) {
                        self.agent_mode = am.mode;
                    }
                }
                vec![]
            }
            "file_index" => {
                if let Some(data) = raw.data {
                    if let Ok(fi) = serde_json::from_value::<FileIndexData>(data) {
                        self.file_index = fi
                            .files
                            .into_iter()
                            .map(|entry| FileIndexEntryLite {
                                path: entry.path,
                                is_dir: entry.is_dir,
                            })
                            .collect();
                        self.file_index_generated_at = Some(fi.generated_at);
                        self.file_index_loading = false;
                        self.file_index_error = None;
                        self.refresh_mention_state();
                    }
                }
                vec![]
            }
            "undo_result" => {
                self.pending_session_op = None;
                self.is_thinking = false;
                if let Some(data) = raw.data {
                    if let Ok(ur) = serde_json::from_value::<UndoResultData>(data) {
                        let message_id_for_files = ur
                            .message_id
                            .clone()
                            .or_else(|| ur.undo_stack.last().map(|frame| frame.message_id.clone()));
                        let next = self.build_undo_state_from_server_stack(
                            &ur.undo_stack,
                            message_id_for_files.as_deref(),
                            if ur.success {
                                Some(ur.reverted_files.as_slice())
                            } else {
                                None
                            },
                        );
                        self.undo_state = next;

                        if ur.success {
                            self.streaming_content.clear();
                            self.streaming_cache.invalidate();
                            self.status = "undone - reloading session".into();
                            if let Some(ref sid) = self.session_id {
                                return vec![ClientMsg::LoadSession {
                                    session_id: sid.clone(),
                                }];
                            }
                        } else {
                            self.status = ur.message.unwrap_or_else(|| "undo failed".into());
                        }
                    }
                }
                vec![]
            }
            "redo_result" => {
                self.pending_session_op = None;
                self.is_thinking = false;
                if let Some(data) = raw.data {
                    if let Ok(rr) = serde_json::from_value::<RedoResultData>(data) {
                        self.undo_state =
                            self.build_undo_state_from_server_stack(&rr.undo_stack, None, None);
                        if rr.success {
                            self.status = "redone - reloading session".into();
                            if let Some(ref sid) = self.session_id {
                                return vec![ClientMsg::LoadSession {
                                    session_id: sid.clone(),
                                }];
                            }
                        } else {
                            self.status = rr.message.unwrap_or_else(|| "redo failed".into());
                        }
                    }
                }
                vec![]
            }
            "session_list" => {
                if let Some(data) = raw.data {
                    if let Ok(list) = serde_json::from_value::<SessionListData>(data) {
                        // Sort sessions within each group by updated_at descending.
                        let mut groups = list.groups;
                        for group in &mut groups {
                            group.sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                        }
                        // Sort groups by their most-recent session activity descending.
                        groups.sort_by(|a, b| {
                            let a_latest = a.sessions.first().and_then(|s| s.updated_at.as_deref());
                            let b_latest = b.sessions.first().and_then(|s| s.updated_at.as_deref());
                            b_latest.cmp(&a_latest)
                        });

                        let total: usize = groups.iter().map(|g| g.sessions.len()).sum();
                        self.session_groups = groups;

                        // Clamp cursor to the new visible item count.
                        let visible_len = self.visible_start_items().len();
                        if self.session_cursor >= visible_len && visible_len > 0 {
                            self.session_cursor = visible_len - 1;
                        }
                        self.status = format!("{} session(s)", total);
                    }
                }
                vec![]
            }
            "session_created" => {
                if let Some(data) = raw.data {
                    if let Ok(sc) = serde_json::from_value::<SessionCreatedData>(data) {
                        self.session_id = Some(sc.session_id.clone());
                        self.agent_id = Some(sc.agent_id);
                        self.messages.clear();
                        self.streaming_content.clear();
                        self.streaming_cache.invalidate();
                        self.scroll_offset = 0;
                        self.undo_state = None;
                        self.undoable_turns.clear();
                        self.file_index.clear();
                        self.file_index_generated_at = None;
                        self.file_index_loading = false;
                        self.file_index_error = None;
                        self.mention_state = None;
                        self.live_compaction = None;
                        self.last_compaction_token_estimate = None;
                        self.elicitation = None;
                        self.clear_cancel_confirm();
                        self.cumulative_cost = None;
                        self.session_stats = SessionStatsLite::default();
                        self.screen = Screen::Chat;
                        self.status = "session created".into();
                        return vec![ClientMsg::SubscribeSession {
                            session_id: sc.session_id,
                            agent_id: self.agent_id.clone(),
                        }];
                    }
                }
                vec![]
            }
            "session_loaded" => {
                if let Some(data) = raw.data {
                    match serde_json::from_value::<SessionLoadedData>(data.clone()) {
                        Err(e) => {
                            self.pending_session_op = None;
                            self.is_thinking = false;
                            self.status = format!("load error: {e}");
                        }
                        Ok(sl) => {
                            self.pending_session_op = None;
                            self.session_id = Some(sl.session_id);
                            self.agent_id = Some(sl.agent_id);
                            self.messages.clear();
                            self.streaming_content.clear();
                            self.streaming_cache.invalidate();
                            self.scroll_offset = 0;
                            self.cumulative_cost = None;
                            self.session_stats = SessionStatsLite::default();
                            self.is_thinking = false;
                            self.screen = Screen::Chat;
                            self.undoable_turns.clear();
                            self.file_index.clear();
                            self.file_index_generated_at = None;
                            self.file_index_loading = false;
                            self.file_index_error = None;
                            self.mention_state = None;
                            self.live_compaction = None;
                            self.last_compaction_token_estimate = None;
                            self.elicitation = None;
                            self.clear_cancel_confirm();
                            self.undo_state =
                                self.build_undo_state_from_server_stack(&sl.undo_stack, None, None);
                            self.status = "ready".into();
                            // Replay audit: sets current_provider/model (ProviderChanged)
                            // and agent_mode (SessionModeChanged).
                            self.replay_audit(&sl.audit);

                            // Restore the session's mode on the server.
                            let mut cmds = vec![ClientMsg::SetAgentMode {
                                mode: self.agent_mode.clone(),
                            }];
                            // Restore cached model + effort for this session + mode.
                            cmds.extend(self.apply_cached_mode_state());
                            return cmds;
                        }
                    }
                }
                vec![]
            }
            "session_events" => {
                if let Some(data) = raw.data {
                    if let Ok(se) = serde_json::from_value::<SessionEventsData>(data) {
                        self.note_session_activity(&se.session_id);
                        if self.session_id.as_deref() == Some(se.session_id.as_str()) {
                            for envelope in se.events {
                                self.handle_event(&envelope);
                            }
                        }
                    }
                }
                vec![]
            }
            "event" => {
                if let Some(data) = raw.data {
                    if let Ok(ed) = serde_json::from_value::<EventData>(data) {
                        self.note_session_activity(&ed.session_id);
                        if self.session_id.as_deref() == Some(ed.session_id.as_str()) {
                            self.handle_event(&ed.event);
                        }
                    }
                }
                vec![]
            }
            "all_models_list" => {
                if let Some(data) = raw.data {
                    if let Ok(ml) = serde_json::from_value::<AllModelsData>(data) {
                        self.models = ml.models;
                    }
                }
                vec![]
            }
            "error" => {
                if let Some(data) = raw.data {
                    if let Ok(e) = serde_json::from_value::<ErrorData>(data) {
                        self.messages.push(ChatEntry::Error(e.message.clone()));
                        self.status = format!("error: {}", e.message);
                    }
                }
                vec![]
            }
            _ => vec![],
        }
    }

    fn handle_event(&mut self, envelope: &EventEnvelope) {
        self.apply_event_stats(envelope.kind(), envelope.timestamp());
        self.handle_event_kind(envelope.kind(), false);
    }

    fn handle_event_kind(&mut self, kind: &EventKind, is_replay: bool) {
        match kind {
            EventKind::PromptReceived {
                content,
                message_id,
            } => {
                let text = content_to_string(content);
                if !text.is_empty() {
                    if !is_replay {
                        if let Some(frontier_message_id) = self
                            .undo_state
                            .as_ref()
                            .and_then(|state| state.frontier_message_id.clone())
                        {
                            if let Some(frontier_idx) = self
                                .messages
                                .iter()
                                .position(|entry| matches!(entry, ChatEntry::User { message_id: Some(mid), .. } if mid == &frontier_message_id))
                            {
                                self.messages.truncate(frontier_idx);
                            }
                            if let Some(turn_idx) = self
                                .undoable_turns
                                .iter()
                                .position(|turn| turn.message_id == frontier_message_id)
                            {
                                self.undoable_turns.truncate(turn_idx);
                            }
                        }
                        self.undo_state = None;
                    }

                    self.messages.push(ChatEntry::User {
                        text: text.clone(),
                        message_id: message_id.clone(),
                    });
                    if let Some(message_id) = message_id.clone() {
                        self.undoable_turns.push(UndoableTurn {
                            turn_id: message_id.clone(),
                            message_id,
                            text,
                        });
                    }
                }
            }
            EventKind::UserMessageStored { content } => {
                let text = content_to_string(content);
                if !text.is_empty() {
                    let dup = matches!(
                        self.messages.last(),
                        Some(ChatEntry::User { text: last, .. }) if last == &text
                    ) || self
                        .undoable_turns
                        .last()
                        .map(|turn| turn.text == text)
                        .unwrap_or(false);
                    if !dup {
                        self.messages.push(ChatEntry::User {
                            text,
                            message_id: None,
                        });
                    }
                }
            }
            EventKind::TurnStarted => {
                self.clear_cancel_confirm();
                self.is_thinking = true;
                self.streaming_content.clear();
                self.streaming_cache.invalidate();
                self.status = "thinking...".into();
            }
            EventKind::AssistantContentDelta { content, .. } => {
                self.streaming_content.push_str(content);
                self.scroll_offset = 0;
            }
            EventKind::CompactionStart { token_estimate } => {
                self.live_compaction = Some(LiveCompactionState {
                    token_estimate: *token_estimate,
                });
                self.last_compaction_token_estimate = Some(*token_estimate);
                self.messages.push(ChatEntry::CompactionStart {
                    token_estimate: *token_estimate,
                });
                self.status = format!("compacting context (~{token_estimate} tokens)");
            }
            EventKind::CompactionEnd {
                summary,
                summary_len,
            } => {
                self.live_compaction = None;
                self.messages
                    .retain(|entry| !matches!(entry, ChatEntry::CompactionStart { .. }));
                self.messages.push(ChatEntry::CompactionEnd {
                    token_estimate: self.last_compaction_token_estimate,
                    summary: summary.clone(),
                    summary_len: *summary_len,
                });
                self.status = "context compacted".into();
            }
            EventKind::AssistantMessageStored { content, .. } => {
                self.is_thinking = false;
                self.streaming_content.clear();
                self.streaming_cache.invalidate();
                if !content.is_empty() {
                    self.messages.push(ChatEntry::Assistant(content.clone()));
                }
            }
            EventKind::ToolCallStart {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                self.status = format!("tool: {tool_name}");
                // The question tool renders as an ElicitationCard — skip the
                // redundant "> question …" tool call entry in the chat.
                if tool_name != "question" {
                    let detail = parse_tool_detail(tool_name, arguments.as_ref());
                    self.messages.push(ChatEntry::ToolCall {
                        tool_call_id: tool_call_id.clone(),
                        name: tool_name.clone(),
                        is_error: false,
                        detail,
                    });
                }
            }
            EventKind::ToolCallEnd {
                tool_call_id,
                tool_name,
                is_error,
                result,
            } => {
                if tool_name == "question" {
                    if is_replay {
                        if let Some(result_str) = result {
                            backfill_elicitation_outcomes(&mut self.messages, result_str);
                        }
                    }
                } else {
                    if let Some(result_str) = result {
                        update_tool_detail(&mut self.messages, tool_call_id.as_deref(), result_str);
                    }
                    if is_error.unwrap_or(false) {
                        self.messages.push(ChatEntry::ToolCall {
                            tool_call_id: tool_call_id.clone(),
                            name: format!("{tool_name} (failed)"),
                            is_error: true,
                            detail: ToolDetail::None,
                        });
                    }
                }
            }
            EventKind::ProviderChanged {
                provider,
                model,
                context_limit,
                ..
            } => {
                self.current_provider = Some(provider.clone());
                self.current_model = Some(model.clone());
                if let Some(limit) = context_limit {
                    self.context_limit = *limit;
                }
                // Keep the session cache in sync with live model changes.
                if !is_replay {
                    self.cache_session_mode_state();
                }
            }
            EventKind::LlmRequestEnd {
                cumulative_cost_usd,
                ..
            } => {
                self.clear_cancel_confirm();
                self.is_thinking = false;
                self.cumulative_cost = *cumulative_cost_usd;
                self.status = "ready".into();
            }
            EventKind::Error { message } => {
                self.pending_session_op = None;
                self.clear_cancel_confirm();
                self.is_thinking = false;
                self.messages.push(ChatEntry::Error(message.clone()));
                self.status = format!("error: {message}");
            }
            EventKind::ElicitationRequested {
                elicitation_id,
                message,
                source,
                requested_schema,
                ..
            } => {
                if is_replay {
                    // During replay the elicitation was already answered —
                    // show the card as resolved without reopening the popup.
                    self.messages.push(ChatEntry::Elicitation {
                        elicitation_id: elicitation_id.clone(),
                        message: message.clone(),
                        source: source.clone(),
                        outcome: Some("responded".into()),
                    });
                    return;
                }
                let fields = ElicitationState::parse_schema(requested_schema);
                self.elicitation = Some(ElicitationState {
                    elicitation_id: elicitation_id.clone(),
                    message: message.clone(),
                    source: source.clone(),
                    fields,
                    field_cursor: 0,
                    option_cursor: 0,
                    selected: HashMap::new(),
                    text_input: String::new(),
                    text_cursor: 0,
                });
                self.messages.push(ChatEntry::Elicitation {
                    elicitation_id: elicitation_id.clone(),
                    message: message.clone(),
                    source: source.clone(),
                    outcome: None,
                });
                self.scroll_offset = 0;
                self.status = "question — answer in the panel above input".into();
            }
            EventKind::SessionModeChanged { mode } => {
                self.agent_mode = mode.clone();
            }
            EventKind::Cancelled => {
                self.pending_session_op = None;
                self.clear_cancel_confirm();
                self.is_thinking = false;
                if !self.streaming_content.is_empty() {
                    let partial = std::mem::take(&mut self.streaming_content);
                    self.streaming_cache.invalidate();
                    self.messages
                        .push(ChatEntry::Assistant(format!("{partial} [cancelled]")));
                }
                self.status = "cancelled".into();
            }
            _ => {}
        }
    }

    fn replay_audit(&mut self, audit: &serde_json::Value) {
        if let Some(events) = audit.get("events").and_then(|e| e.as_array()) {
            let frontier_message_id = self
                .undo_state
                .as_ref()
                .and_then(|state| state.frontier_message_id.as_deref());
            let mut replay_cutoff = events.len();

            if let Some(frontier_message_id) = frontier_message_id {
                if let Some(idx) = events.iter().position(|event_val| {
                    serde_json::from_value::<AgentEvent>(event_val.clone())
                        .ok()
                        .and_then(|event| match event.kind {
                            EventKind::PromptReceived {
                                message_id: Some(message_id),
                                ..
                            } => Some(message_id == frontier_message_id),
                            _ => None,
                        })
                        .unwrap_or(false)
                }) {
                    replay_cutoff = idx;
                }
            }

            for event_val in events.iter().take(replay_cutoff) {
                if let Ok(agent_event) = serde_json::from_value::<AgentEvent>(event_val.clone()) {
                    self.apply_event_stats(&agent_event.kind, agent_event.timestamp);
                    self.handle_event_kind(&agent_event.kind, true);
                }
            }
        }
    }

    // -- input helpers --

    pub fn input_insert(&mut self, c: char) {
        self.input.insert(self.input_cursor, c);
        self.input_cursor += c.len_utf8();
        self.refresh_mention_state();
    }

    pub fn input_backspace(&mut self) {
        if self.input_cursor > 0 {
            let prev = self.input[..self.input_cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.input.drain(prev..self.input_cursor);
            self.input_cursor = prev;
            self.refresh_mention_state();
        }
    }

    pub fn input_delete(&mut self) {
        if self.input_cursor < self.input.len() {
            let next = self.input[self.input_cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.input_cursor + i)
                .unwrap_or(self.input.len());
            self.input.drain(self.input_cursor..next);
            self.refresh_mention_state();
        }
    }

    pub fn input_left(&mut self) {
        if self.input_cursor > 0 {
            self.input_cursor = self.input[..self.input_cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.refresh_mention_state();
        }
    }

    pub fn input_right(&mut self) {
        if self.input_cursor < self.input.len() {
            self.input_cursor = self.input[self.input_cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.input_cursor + i)
                .unwrap_or(self.input.len());
            self.refresh_mention_state();
        }
    }

    pub fn input_home(&mut self) {
        self.input_cursor = 0;
        self.refresh_mention_state();
    }

    pub fn input_end(&mut self) {
        self.input_cursor = self.input.len();
        self.refresh_mention_state();
    }

    pub fn active_mention_query_from(&self, input: &str, cursor: usize) -> Option<(usize, String)> {
        if cursor > input.len() || !input.is_char_boundary(cursor) {
            return None;
        }

        let before_cursor = &input[..cursor];
        let trigger_start = before_cursor.rfind('@')?;
        let prefix = &before_cursor[..trigger_start];
        if !prefix.is_empty() && !prefix.ends_with(char::is_whitespace) {
            return None;
        }

        let token = &before_cursor[trigger_start + 1..];
        if token.chars().any(char::is_whitespace) {
            return None;
        }

        Some((trigger_start, token.to_string()))
    }

    pub fn rank_file_matches(&self, query: &str) -> Vec<FileIndexEntryLite> {
        let matcher = SkimMatcherV2::default();
        let mut scored: Vec<(i64, bool, usize, &FileIndexEntryLite)> = self
            .file_index
            .iter()
            .filter_map(|entry| {
                let path = entry.path.as_str();
                let filename = path.rsplit('/').next().unwrap_or(path);
                let lower_path = path.to_lowercase();
                let lower_filename = filename.to_lowercase();
                let lower_query = query.to_lowercase();

                let mut score = matcher.fuzzy_match(path, query)?;
                if query.is_empty() {
                    score = 0;
                }
                if !query.is_empty() && lower_path.starts_with(&lower_query) {
                    score += 10_000;
                }
                if !query.is_empty() && lower_filename.starts_with(&lower_query) {
                    score += 7_500;
                }
                if !query.is_empty() && lower_path.contains(&lower_query) {
                    score += 3_000;
                }

                Some((score, entry.is_dir, path.len(), entry))
            })
            .collect();

        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.3.path.cmp(&b.3.path))
        });

        scored
            .into_iter()
            .take(8)
            .map(|(_, _, _, entry)| entry.clone())
            .collect()
    }

    pub fn refresh_mention_state(&mut self) {
        let Some((trigger_start, query)) =
            self.active_mention_query_from(&self.input, self.input_cursor)
        else {
            self.mention_state = None;
            return;
        };

        let results = self.rank_file_matches(&query);
        self.mention_state = Some(MentionState {
            trigger_start,
            query,
            selected_index: 0,
            results,
        });
    }

    pub fn request_file_index_if_needed(&mut self) -> Option<ClientMsg> {
        if self.mention_state.is_some() && self.file_index.is_empty() && !self.file_index_loading {
            self.file_index_loading = true;
            self.file_index_error = None;
            return Some(ClientMsg::GetFileIndex);
        }
        None
    }

    pub fn move_mention_selection(&mut self, delta: isize) {
        if let Some(mention) = self.mention_state.as_mut() {
            let len = mention.results.len();
            if len == 0 {
                mention.selected_index = 0;
                return;
            }
            let next = (mention.selected_index as isize + delta).rem_euclid(len as isize) as usize;
            mention.selected_index = next;
        }
    }

    pub fn accept_selected_mention(&mut self) -> bool {
        let Some(mention) = self.mention_state.clone() else {
            return false;
        };
        let Some(selected) = mention.results.get(mention.selected_index).cloned() else {
            return false;
        };

        let replacement = format!("@{} ", selected.path);
        let replace_end = mention.trigger_start + 1 + mention.query.len();
        self.input
            .replace_range(mention.trigger_start..replace_end, &replacement);
        self.input_cursor = mention.trigger_start + replacement.len();
        self.mention_state = None;
        true
    }

    pub fn build_prompt_text_and_links(&self, input: &str) -> (String, Vec<String>) {
        let mut links = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let bytes = input.as_bytes();
        let mut i = 0usize;

        while i < bytes.len() {
            if bytes[i] == b'@' {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() {
                    let ch = input[end..].chars().next().unwrap_or(' ');
                    if ch.is_whitespace() {
                        break;
                    }
                    end += ch.len_utf8();
                }
                if end > start {
                    let candidate = &input[start..end];
                    let looks_like_path = candidate.contains('/') || candidate.contains('.');
                    if looks_like_path && seen.insert(candidate.to_string()) {
                        links.push(candidate.to_string());
                    }
                }
                i = end.max(i + 1);
                continue;
            }
            i += 1;
        }

        (input.to_string(), links)
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

    pub fn is_busy_for_input(&self) -> bool {
        self.is_thinking || self.pending_session_op.is_some() || self.elicitation.is_some()
    }

    pub fn pending_session_label(&self) -> Option<&'static str> {
        match self.pending_session_op {
            Some(SessionOp::Undo) => Some("undoing"),
            Some(SessionOp::Redo) => Some("redoing"),
            None => None,
        }
    }

    pub fn current_undo_target(&self) -> Option<&UndoableTurn> {
        let frontier_message_id = self
            .undo_state
            .as_ref()
            .and_then(|state| state.frontier_message_id.as_deref());

        let mut start_index = self.undoable_turns.len();
        if let Some(frontier_message_id) = frontier_message_id {
            if let Some(frontier_index) = self
                .undoable_turns
                .iter()
                .position(|turn| turn.message_id == frontier_message_id)
            {
                start_index = frontier_index;
            }
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
            {
                if eid == elicitation_id {
                    *out = Some(outcome.to_string());
                    break;
                }
            }
        }
        self.elicitation = None;
        self.card_cache.invalidate();
        self.status = "ready".into();
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

    pub fn take_input(&mut self) -> String {
        self.input_cursor = 0;
        self.input_scroll = 0;
        self.scroll_offset = 0;
        self.mention_state = None;
        std::mem::take(&mut self.input)
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
        assert_eq!(app.resolve_new_session_default_cwd().as_deref(), Some("/session"));

        app.session_groups[0].sessions[0].cwd = None;
        assert_eq!(app.resolve_new_session_default_cwd().as_deref(), Some("/group"));

        app.session_groups.clear();
        assert_eq!(app.resolve_new_session_default_cwd().as_deref(), Some("/launch"));
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
            app.normalize_new_session_path("../proj/./subdir/..",).as_deref(),
            Some("/launch/proj")
        );
        assert_eq!(
            app.normalize_new_session_path("/absolute/path/../clean").as_deref(),
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
        assert!(results.iter().any(|entry| entry.path.ends_with("project-dir")));
        assert!(!results.iter().any(|entry| entry.path.ends_with("project-file.txt")));
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

        assert_eq!(app.session_cache["s1"]["build"].model, "anthropic/claude-sonnet");
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
            CachedModeState { model: "anthropic/claude-sonnet".into(), effort: Some("high".into()) },
        );

        let cmds = app.apply_cached_mode_state();
        assert_eq!(app.reasoning_effort, Some("high".into()));
        assert_eq!(cmds.len(), 1);
        assert!(matches!(&cmds[0], ClientMsg::SetReasoningEffort { reasoning_effort } if reasoning_effort == "high"));
    }

    #[test]
    fn apply_restores_model_and_effort_when_model_differs() {
        let mut app = App::new();
        set_ctx(&mut app, "s1", "build", "anthropic", "claude-sonnet");
        app.reasoning_effort = None;
        // The cached state says build mode used opus with max effort
        app.session_cache.entry("s1".into()).or_default().insert(
            "build".into(),
            CachedModeState { model: "anthropic/claude-opus".into(), effort: Some("max".into()) },
        );
        // Need the model in the models list for the lookup
        app.models = vec![make_model_entry("anthropic", "claude-opus")];

        let cmds = app.apply_cached_mode_state();

        assert_eq!(app.current_provider.as_deref(), Some("anthropic"));
        assert_eq!(app.current_model.as_deref(), Some("claude-opus"));
        assert_eq!(app.reasoning_effort, Some("max".into()));
        assert_eq!(cmds.len(), 2);
        assert!(matches!(&cmds[0], ClientMsg::SetSessionModel { .. }));
        assert!(matches!(&cmds[1], ClientMsg::SetReasoningEffort { reasoning_effort } if reasoning_effort == "max"));
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
            CachedModeState { model: "anthropic/claude-sonnet".into(), effort: Some("high".into()) },
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
            CachedModeState { model: "anthropic/claude-opus".into(), effort: Some("max".into()) },
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
        app.handle_event_kind(&EventKind::SessionModeChanged { mode: "plan".into() }, false);
        assert_eq!(app.agent_mode, "plan");
    }

    #[test]
    fn live_session_mode_changed_to_build_updates_agent_mode() {
        let mut app = App::new();
        app.agent_mode = "plan".into();
        app.handle_event_kind(&EventKind::SessionModeChanged { mode: "build".into() }, false);
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
            cmds.iter().any(|m| matches!(m, ClientMsg::SetSessionModel { .. })),
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
            !cmds.iter().any(|m| matches!(m, ClientMsg::SetReasoningEffort { .. })),
            "expected no SetReasoningEffort: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|m| matches!(m, ClientMsg::SetSessionModel { .. })),
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
            app.live_compaction,
            Some(LiveCompactionState {
                token_estimate: 12_000
            })
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

        assert_eq!(app.live_compaction, None);
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
        app.live_compaction = Some(LiveCompactionState {
            token_estimate: 9_000,
        });
        assert_eq!(app.pending_session_label(), None);

        app.pending_session_op = Some(SessionOp::Undo);
        assert_eq!(app.pending_session_label(), Some("undoing"));
    }

    #[test]
    fn cancel_confirm_arms_expires_and_restores_status() {
        let mut app = App::new();
        app.is_thinking = true;

        app.arm_cancel_confirm();
        assert!(app.cancel_confirm_active());
        assert_eq!(app.status, "press Esc again to stop");

        app.pending_cancel_confirm_until = Some(Instant::now() - Duration::from_millis(1));
        app.clear_expired_cancel_confirm();
        assert!(!app.cancel_confirm_active());
        assert_eq!(app.status, "thinking...");
    }

    #[test]
    fn refresh_transient_status_preserves_connection_and_operation_precedence() {
        let mut app = App::new();
        app.conn = ConnState::Disconnected;
        app.status = "connection lost - retrying".into();
        app.refresh_transient_status();
        assert_eq!(app.status, "connection lost - retrying");

        app.conn = ConnState::Connected;
        app.is_thinking = true;
        app.refresh_transient_status();
        assert_eq!(app.status, "thinking...");

        app.live_compaction = Some(LiveCompactionState {
            token_estimate: 2048,
        });
        app.refresh_transient_status();
        assert_eq!(app.status, "compacting context (~2048 tokens)");

        app.pending_session_op = Some(SessionOp::Redo);
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
    fn pending_session_op_blocks_input_and_reports_label() {
        let mut app = App::new();
        assert!(!app.is_busy_for_input());
        assert_eq!(app.pending_session_label(), None);

        app.pending_session_op = Some(SessionOp::Undo);
        assert!(app.is_busy_for_input());
        assert_eq!(app.pending_session_label(), Some("undoing"));

        app.pending_session_op = Some(SessionOp::Redo);
        assert!(app.is_busy_for_input());
        assert_eq!(app.pending_session_label(), Some("redoing"));

        app.pending_session_op = None;
        app.is_thinking = true;
        assert!(app.is_busy_for_input());
        assert_eq!(app.pending_session_label(), None);
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

        app.handle_connection_event(ConnectionEvent::Disconnected {
            reason: "socket closed".into(),
        });
        assert_eq!(app.conn, ConnState::Disconnected);
        assert_eq!(app.reconnect_delay_ms, None);
        assert_eq!(app.status, "connection lost - socket closed");

        app.session_id = Some("session-1".into());
        app.handle_connection_event(ConnectionEvent::Connected);
        assert_eq!(app.conn, ConnState::Connected);
        assert_eq!(app.reconnect_attempt, 0);
        assert_eq!(app.reconnect_delay_ms, None);
        assert_eq!(app.status, "reconnected");
    }

    #[test]
    fn undo_and_redo_results_clear_pending_session_op() {
        let mut app = App::new();
        app.pending_session_op = Some(SessionOp::Undo);
        app.handle_server_msg(RawServerMsg {
            msg_type: "undo_result".into(),
            data: Some(serde_json::json!({
                "success": false,
                "message": "undo failed",
                "undo_stack": []
            })),
        });
        assert_eq!(app.pending_session_op, None);

        app.pending_session_op = Some(SessionOp::Redo);
        app.handle_server_msg(RawServerMsg {
            msg_type: "redo_result".into(),
            data: Some(serde_json::json!({
                "success": false,
                "message": "redo failed",
                "undo_stack": []
            })),
        });
        assert_eq!(app.pending_session_op, None);
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
        assert!(matches!(&app.messages[1], ChatEntry::Assistant(text) if text == "reply one"));
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
        assert!(matches!(&items[0], StartPageItem::GroupHeader { collapsed: true, .. }));
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
        assert!(matches!(&items[0], StartPageItem::GroupHeader { collapsed: true, .. }));
        assert!(matches!(&items[1], StartPageItem::GroupHeader { collapsed: false, .. }));
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
        assert!(matches!(&items[1], StartPageItem::Session { group_idx: 0, session_idx: 0 }));
        assert!(matches!(&items[2], StartPageItem::Session { group_idx: 0, session_idx: 1 }));
        assert!(matches!(&items[4], StartPageItem::Session { group_idx: 1, session_idx: 0 }));
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
        assert_eq!(app.session_groups[0].cwd.as_deref(), Some("/home/user/proj"));
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
        app.session_groups = vec![make_group(
            Some("/a"),
            &[("aaa", None), ("bbb", None)],
        )];
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
            StartPageItem::GroupHeader { session_count: 3, .. }
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
        app.session_groups = vec![make_group(Some("/a"), &[
            ("s1", None), ("s2", None), ("s3", None),
        ])];
        let items = app.visible_start_items();
        // header + 3 sessions, no ShowMore
        assert_eq!(items.len(), 4);
        assert!(!items.iter().any(|i| matches!(i, StartPageItem::ShowMore { .. })));
    }

    #[test]
    fn visible_items_group_with_four_sessions_caps_at_three_plus_show_more() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &[
            ("s1", None), ("s2", None), ("s3", None), ("s4", None),
        ])];
        let items = app.visible_start_items();
        // header + 3 sessions + ShowMore
        assert_eq!(items.len(), 5);
        assert!(matches!(items[4], StartPageItem::ShowMore { remaining: 1, .. }));
    }

    #[test]
    fn visible_items_show_more_remaining_is_total_minus_three() {
        let mut app = App::new();
        // 7 sessions → show 3 + ShowMore(remaining=4)
        app.session_groups = vec![make_group(Some("/a"), &[
            ("s1", None), ("s2", None), ("s3", None),
            ("s4", None), ("s5", None), ("s6", None), ("s7", None),
        ])];
        let items = app.visible_start_items();
        assert!(matches!(items.last(), Some(StartPageItem::ShowMore { remaining: 4, .. })));
    }

    #[test]
    fn visible_items_filter_active_bypasses_cap() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &[
            ("aaa1", None), ("aaa2", None), ("aaa3", None), ("aaa4", None),
        ])];
        app.session_filter = "aaa".to_string();
        let items = app.visible_start_items();
        // All 4 match the filter → header + 4 sessions, no ShowMore
        assert_eq!(items.len(), 5);
        assert!(!items.iter().any(|i| matches!(i, StartPageItem::ShowMore { .. })));
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
        assert!(!items.iter().any(|i| matches!(i, StartPageItem::ShowMore { .. })));
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
        assert!(matches!(items.last(), Some(StartPageItem::ShowMore { remaining: 1 })));
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
        assert!(matches!(items.last(), Some(StartPageItem::ShowMore { remaining: 3 })));
    }

    #[test]
    fn visible_items_group_cap_filter_active_bypasses() {
        let mut app = App::new();
        app.session_groups = vec![
            make_group(Some("/a"), &[("aaa1", None)]),
            make_group(Some("/b"), &[("aaa2", None)]),
            make_group(Some("/c"), &[("aaa3", None)]),
            make_group(Some("/d"), &[("aaa4", None)]),
        ];
        app.session_filter = "aaa".to_string();
        let items = app.visible_start_items();
        // Filter active → all 4 groups shown, no trailing ShowMore
        let headers = items.iter().filter(|i| matches!(i, StartPageItem::GroupHeader { .. })).count();
        assert_eq!(headers, 4);
        assert!(!items.iter().any(|i| matches!(i, StartPageItem::ShowMore { .. })));
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
        assert!(!items.iter().any(|i| matches!(i, PopupItem::GroupHeader { .. } if false)));
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
        let headers = items.iter().filter(|i| matches!(i, PopupItem::GroupHeader { .. })).count();
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
        assert!(matches!(&items[0], PopupItem::GroupHeader { collapsed: true, .. }));
    }

    #[test]
    fn popup_expanded_shows_sessions() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1"])];
        // Not in popup_collapsed_groups → expanded
        let items = app.visible_popup_items();
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], PopupItem::GroupHeader { collapsed: false, .. }));
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
        assert!(matches!(&items[0], PopupItem::GroupHeader { collapsed: true, .. }));
        assert!(matches!(&items[1], PopupItem::GroupHeader { collapsed: false, .. }));
    }

    // ── filter hides non-matching sessions ────────────────────────────────────

    #[test]
    fn popup_filter_hides_non_matching_sessions() {
        let mut app = App::new();
        app.session_groups = vec![make_group(
            Some("/a"),
            &["aaa", "bbb", "aab"],
        )];
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
        assert!(matches!(&items[1], PopupItem::Session { group_idx: 0, session_idx: 0 }));
        assert!(matches!(&items[2], PopupItem::Session { group_idx: 0, session_idx: 1 }));
        assert!(matches!(&items[4], PopupItem::Session { group_idx: 1, session_idx: 0 }));
    }

    // ── group header carries correct session_count ────────────────────────────

    #[test]
    fn popup_group_header_session_count_reflects_total() {
        let mut app = App::new();
        app.session_groups = vec![make_group(Some("/a"), &["s1", "s2", "s3"])];
        let items = app.visible_popup_items();
        assert!(matches!(
            &items[0],
            PopupItem::GroupHeader { session_count: 3, .. }
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

/// Backfill `ChatEntry::Elicitation` cards that were pushed with a generic
/// `"responded"` outcome during replay, replacing it with the actual answer
/// labels extracted from the `question` tool's `ToolCallEnd` result JSON.
///
/// The result format is:
/// `{ "answers": [{ "question": "...", "answers": ["label1", ...] }] }`
///
/// Cards are matched to answer entries in document order — the same order the
/// server asked the questions.  Cards whose outcome is already something other
/// than `"responded"` are skipped (they were resolved in a previous backfill).
fn backfill_elicitation_outcomes(messages: &mut Vec<ChatEntry>, result_str: &str) {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(result_str) else {
        return;
    };
    let Some(answers) = val.get("answers").and_then(|a| a.as_array()) else {
        return;
    };

    let mut answer_iter = answers.iter();
    for entry in messages.iter_mut() {
        let ChatEntry::Elicitation { outcome, .. } = entry else {
            continue;
        };
        if outcome.as_deref() != Some("responded") {
            continue;
        }
        let Some(answer_entry) = answer_iter.next() else {
            break;
        };
        let labels: Vec<String> = answer_entry
            .get("answers")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| format!("{OUTCOME_BULLET}{s}"))
                    .collect()
            })
            .unwrap_or_default();
        *outcome = Some(labels.join("\n"));
    }
}

fn parse_tool_detail(tool_name: &str, arguments: Option<&serde_json::Value>) -> ToolDetail {
    let Some(args) = arguments else {
        return ToolDetail::None;
    };
    // arguments can be a JSON string or an object
    let obj = if let Some(s) = args.as_str() {
        serde_json::from_str::<serde_json::Value>(s).unwrap_or_default()
    } else {
        args.clone()
    };

    let str_field = |key: &str| -> String {
        obj.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let short = |path: &str| -> String {
        let mut count = 0;
        for (i, c) in path.char_indices().rev() {
            if c == '/' {
                count += 1;
                if count == 2 {
                    return path[i + 1..].to_string();
                }
            }
        }
        path.to_string()
    };

    match tool_name {
        "edit" => {
            let file = obj
                .get("filePath")
                .or_else(|| obj.get("file_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let old = obj
                .get("oldString")
                .or_else(|| obj.get("old_string"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let new = obj
                .get("newString")
                .or_else(|| obj.get("new_string"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let cached_lines = build_diff_lines(&old, &new, None);
            ToolDetail::Edit {
                file,
                old,
                new,
                start_line: None,
                cached_lines,
            }
        }
        "multiedit" => {
            let file = obj
                .get("filePath")
                .or_else(|| obj.get("file_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let count = obj
                .get("edits")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            ToolDetail::Summary(format!("{} ({} edits)", short(file), count))
        }
        "write_file" => {
            let path = str_field("path");
            let content = str_field("content");
            let cached_lines = build_write_lines(&content);
            ToolDetail::WriteFile {
                path,
                content,
                cached_lines,
            }
        }
        "read_tool" => {
            let path = str_field("path");
            let offset = obj.get("offset").and_then(|v| v.as_u64());
            let limit = obj.get("limit").and_then(|v| v.as_u64());
            let range = match (offset, limit) {
                (Some(o), Some(l)) => format!(":{}-{}", o, o + l),
                (Some(o), None) => format!(":{}", o),
                _ => String::new(),
            };
            ToolDetail::Summary(format!("{}{range}", short(&path)))
        }
        "shell" => {
            let cmd = str_field("command");
            let display = if cmd.len() > 60 {
                format!("{}{ELLIPSIS}", &cmd[..60])
            } else {
                cmd
            };
            ToolDetail::Summary(display)
        }
        "search_text" => {
            let pattern = str_field("pattern");
            let path = str_field("path");
            let include = str_field("include");
            let location = if !include.is_empty() {
                include
            } else if !path.is_empty() {
                short(&path).to_string()
            } else {
                ".".into()
            };
            ToolDetail::Summary(format!("\"{}\" {}", pattern, location))
        }
        "glob" => {
            let pattern = str_field("pattern");
            let path = str_field("path");
            if path.is_empty() {
                ToolDetail::Summary(pattern)
            } else {
                ToolDetail::Summary(format!("{} in {}", pattern, short(&path)))
            }
        }
        "ls" => {
            let path = str_field("path");
            ToolDetail::Summary(if path.is_empty() {
                ".".into()
            } else {
                short(&path).to_string()
            })
        }
        "delete_file" => {
            let path = str_field("path");
            ToolDetail::Summary(short(&path).to_string())
        }
        "browse" | "web_fetch" => {
            let url = str_field("url");
            let display = if url.len() > 60 {
                format!("{}{ELLIPSIS}", &url[..60])
            } else {
                url
            };
            ToolDetail::Summary(display)
        }
        "apply_patch" => ToolDetail::Summary("patch".into()),
        "delegate" => {
            let objective = str_field("objective");
            let display = if objective.len() > 50 {
                format!("{}{ELLIPSIS}", &objective[..50])
            } else {
                objective
            };
            ToolDetail::Summary(display)
        }
        "language_query" => {
            let action = str_field("action");
            let uri = str_field("uri");
            ToolDetail::Summary(format!("{} {}", action, short(&uri)))
        }
        "question" => ToolDetail::Summary("asking...".into()),
        "todowrite" => {
            if let Some(todos) = obj.get("todos").and_then(|v| v.as_array()) {
                let lines: Vec<String> = todos
                    .iter()
                    .filter_map(|t| {
                        let content = t.get("content").and_then(|v| v.as_str()).unwrap_or("");
                        let status = t
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("pending");
                        if content.is_empty() {
                            return None;
                        }
                        let check = if status == "completed" { "x" } else { " " };
                        Some(format!("[{check}] {content}"))
                    })
                    .collect();
                if lines.is_empty() {
                    ToolDetail::None
                } else {
                    ToolDetail::Summary(lines.join("\n"))
                }
            } else {
                ToolDetail::None
            }
        }
        _ => ToolDetail::None,
    }
}

fn update_tool_detail(messages: &mut [ChatEntry], tool_call_id: Option<&str>, result: &str) {
    let Some(id) = tool_call_id else { return };
    // parse result JSON
    let obj: serde_json::Value = match serde_json::from_str(result) {
        Ok(v) => v,
        Err(_) => return,
    };

    // walk backwards to find matching ToolCall
    for entry in messages.iter_mut().rev() {
        if let ChatEntry::ToolCall {
            tool_call_id: Some(tid),
            name,
            detail,
            ..
        } = entry
        {
            if tid != id {
                continue;
            }
            // edit tool: update start_line
            if let ToolDetail::Edit { start_line: sl, .. } = detail {
                *sl = obj
                    .get("startLineOld")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize);
            }
            // shell tool: show last 3 lines of stdout below command
            if name.starts_with("shell") {
                if let Some(stdout) = obj.get("stdout").and_then(|v| v.as_str()) {
                    let tail: Vec<&str> = stdout
                        .lines()
                        .rev()
                        .filter(|l| !l.trim().is_empty())
                        .take(3)
                        .collect();
                    if !tail.is_empty() {
                        let tail_str = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
                        if let ToolDetail::Summary(header) = detail {
                            *detail = ToolDetail::SummaryWithOutput {
                                header: std::mem::take(header),
                                output: tail_str,
                            };
                        }
                    }
                }
            }
            break;
        }
    }
}

fn content_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    block.get("text").and_then(|t| t.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}
