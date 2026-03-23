use serde::{Deserialize, Serialize};

// --- Client → Server messages ---

#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientMsg {
    Init,
    ListSessions,
    SetReasoningEffort {
        reasoning_effort: String,
    },
    NewSession {
        cwd: Option<String>,
        request_id: Option<String>,
    },
    LoadSession {
        session_id: String,
    },
    Prompt {
        prompt: Vec<PromptBlock>,
    },
    CancelSession,
    ListAllModels {
        refresh: bool,
    },
    SetSessionModel {
        session_id: String,
        model_id: String,
        node_id: Option<String>,
    },
    SubscribeSession {
        session_id: String,
        agent_id: Option<String>,
    },
    DeleteSession {
        session_id: String,
    },
    Undo {
        message_id: String,
    },
    Redo,
    GetFileIndex,
    SetAgentMode {
        mode: String,
    },
    GetAgentMode,
    ElicitationResponse {
        elicitation_id: String,
        action: String, // "accept", "decline", "cancel"
        content: Option<serde_json::Value>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum PromptBlock {
    Text { text: String },
    ResourceLink { name: String, uri: String },
}

// --- Server → Client messages ---
// We use a loose approach: parse the "type" tag, then decode known fields.

#[derive(Debug, Deserialize)]
pub struct RawServerMsg {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub data: Option<serde_json::Value>,
}

// Structured types we extract from RawServerMsg.data

#[derive(Debug, Deserialize)]
pub struct StateData {
    pub active_session_id: Option<String>,
    pub agents: Vec<AgentInfo>,
    pub agent_mode: Option<String>,
    /// Current reasoning effort level. `None` means "auto". Absent key means
    /// the server did not report it — callers should leave existing state intact.
    #[serde(default, deserialize_with = "deserialize_reasoning_effort")]
    pub reasoning_effort: ReasoningEffortField,
}

/// Three-state field for `reasoning_effort` in the `state` message:
/// - `Absent` — key was not present in JSON (leave existing TUI state alone)
/// - `Auto`   — key was `null` or `"auto"` (set to None / auto)
/// - `Set(s)` — key was a non-auto string like `"low"`, `"high"`, etc.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ReasoningEffortField {
    #[default]
    Absent,
    Auto,
    Set(String),
}

fn deserialize_reasoning_effort<'de, D>(d: D) -> Result<ReasoningEffortField, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let v = Option::<String>::deserialize(d)?;
    Ok(match v.as_deref() {
        None | Some("auto") => ReasoningEffortField::Auto,
        Some(s) => ReasoningEffortField::Set(s.to_string()),
    })
}

#[derive(Debug, Deserialize)]
pub struct ReasoningEffortData {
    /// `None` or `"auto"` both map to the "auto" (no effort override) state.
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct SessionCreatedData {
    pub agent_id: String,
    pub session_id: String,
    pub request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SessionListData {
    pub groups: Vec<SessionGroup>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionGroup {
    pub cwd: Option<String>,
    pub sessions: Vec<SessionSummary>,
    /// ISO 8601 timestamp of the most recent activity in this group.
    #[serde(default)]
    pub latest_activity: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: Option<String>,
    /// Working directory for this session (may differ from group cwd for remote sessions).
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Parent session ID if this is a forked session.
    #[serde(default)]
    pub parent_session_id: Option<String>,
    /// Whether this session has child (forked) sessions.
    #[serde(default)]
    pub has_children: bool,
}

#[derive(Debug, Deserialize)]
pub struct SessionLoadedData {
    pub session_id: String,
    pub agent_id: String,
    pub audit: serde_json::Value,
    #[serde(default)]
    pub undo_stack: Vec<UndoStackFrame>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FileIndexEntry {
    pub path: String,
    pub is_dir: bool,
}

#[derive(Debug, Deserialize)]
pub struct FileIndexData {
    pub files: Vec<FileIndexEntry>,
    pub generated_at: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UndoStackFrame {
    pub message_id: String,
}

#[derive(Debug, Deserialize)]
pub struct UndoResultData {
    pub success: bool,
    pub message_id: Option<String>,
    #[serde(default)]
    pub reverted_files: Vec<String>,
    pub message: Option<String>,
    #[serde(default)]
    pub undo_stack: Vec<UndoStackFrame>,
}

#[derive(Debug, Deserialize)]
pub struct RedoResultData {
    pub success: bool,
    pub message: Option<String>,
    #[serde(default)]
    pub undo_stack: Vec<UndoStackFrame>,
}

#[derive(Debug, Deserialize)]
pub struct EventData {
    pub agent_id: String,
    pub session_id: String,
    pub event: EventEnvelope,
}

#[derive(Debug, Deserialize)]
pub struct SessionEventsData {
    pub session_id: String,
    pub agent_id: String,
    pub events: Vec<EventEnvelope>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventEnvelope {
    Durable(InnerEvent),
    Ephemeral(InnerEvent),
}

impl EventEnvelope {
    pub fn kind(&self) -> &EventKind {
        match self {
            Self::Durable(e) | Self::Ephemeral(e) => &e.kind,
        }
    }

    pub fn timestamp(&self) -> Option<i64> {
        match self {
            Self::Durable(e) | Self::Ephemeral(e) => e.timestamp,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct InnerEvent {
    pub kind: EventKind,
    pub timestamp: Option<i64>,
}

/// Flat event shape used in AuditView.events (not wrapped in EventEnvelope).
#[derive(Debug, Deserialize)]
pub struct AgentEvent {
    pub kind: EventKind,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventKind {
    TurnStarted,
    PromptReceived {
        content: serde_json::Value,
        message_id: Option<String>,
    },
    UserMessageStored {
        content: serde_json::Value,
    },
    AssistantMessageStored {
        content: String,
        thinking: Option<String>,
        message_id: Option<String>,
    },
    AssistantContentDelta {
        content: String,
        message_id: Option<String>,
    },
    AssistantThinkingDelta {
        content: String,
        message_id: Option<String>,
    },
    CompactionStart {
        token_estimate: u32,
    },
    CompactionEnd {
        summary: String,
        summary_len: u32,
    },
    LlmRequestStart {
        message_count: Option<u32>,
    },
    LlmRequestEnd {
        finish_reason: Option<String>,
        cost_usd: Option<f64>,
        cumulative_cost_usd: Option<f64>,
        context_tokens: Option<u64>,
        tool_calls: Option<u32>,
        metrics: Option<serde_json::Value>,
    },
    ToolCallStart {
        tool_call_id: Option<String>,
        tool_name: String,
        arguments: Option<serde_json::Value>,
    },
    ToolCallEnd {
        tool_call_id: Option<String>,
        tool_name: String,
        is_error: Option<bool>,
        result: Option<String>,
    },
    ProviderChanged {
        provider: String,
        model: String,
        config_id: Option<i64>,
        context_limit: Option<u64>,
    },
    ElicitationRequested {
        elicitation_id: String,
        session_id: String,
        message: String,
        requested_schema: serde_json::Value,
        source: String,
    },
    /// Emitted when a session's mode changes (per-session mode in actor model).
    /// Durable — appears in the audit journal and replayed on session load.
    /// The last occurrence in a session's audit gives the session's last-used mode.
    SessionModeChanged {
        mode: String,
    },
    Error {
        message: String,
    },
    Cancelled,
    SessionCreated,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct AllModelsData {
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub label: String,
    pub provider: String,
    pub model: String,
    pub node_id: Option<String>,
    pub family: Option<String>,
    pub quant: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AgentModeData {
    pub mode: String,
}

#[derive(Debug, Deserialize)]
pub struct ErrorData {
    pub message: String,
}
