use std::collections::HashMap;

pub(crate) struct SessionState {
    pub(crate) session_id: String,
    pub(crate) trace_id: String,
    pub(crate) root_id: String,
    pub(crate) turn_id: Option<String>,
    pub(crate) turn_start_ns: Option<u64>,
    pub(crate) pairs: HashMap<String, StartState>,
    pub(crate) turn_prompt: Option<String>,
    pub(crate) messages: HashMap<String, MessageStream>,
    pub(crate) usage_offset: u64,
    pub(crate) start_ns: u64,
    pub(crate) last_seen_ns: u64,
    pub(crate) turn_count: u64,
    pub(crate) tool_count: u64,
    pub(crate) subagent_count: u64,
    pub(crate) model: Option<String>,
}

pub(crate) struct StartState {
    pub(crate) span_id: String,
    pub(crate) start_ns: u64,
}

pub(crate) struct MessageStream {
    pub(crate) start_ns: u64,
    pub(crate) chunks: Vec<MessageChunk>,
}

#[derive(Clone)]
pub(crate) struct MessageChunk {
    pub(crate) index: u64,
    pub(crate) time_ns: u64,
    pub(crate) final_chunk: bool,
    pub(crate) delta: String,
}
