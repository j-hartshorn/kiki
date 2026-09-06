use std::time::Duration;

pub(crate) const ACCEPTED_MODEL_CONTENT_LIMIT: usize = 16 * 1024 * 1024;
pub(crate) const PROVIDER_CODE_LIMIT: usize = 256;
pub(crate) const PROVIDER_MESSAGE_LIMIT: usize = 4 * 1024;
pub(crate) const ARENA_EXECUTION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub(crate) const ARENA_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_PROMPT_BYTES: usize = 64 * 1024;
pub(crate) const FILE_MAX_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const COLLECTION_MAX_ITEMS: usize = 10_000;
pub(crate) const COLLECTION_MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const COMMAND_STREAM_MAX_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const TOOL_CONCURRENCY_LIMIT: usize = 16;
pub(crate) const DEFAULT_SUBAGENT_ALLOW_NESTED: bool = true;
pub(crate) const DEFAULT_SUBAGENT_MAX_CONCURRENT: usize = 8;
pub(crate) const DEFAULT_SUBAGENT_MAX_STARTS_PER_RUN: usize = 64;
pub(crate) const DEFAULT_SUBAGENT_MAX_DEPTH: usize = 4;
pub(crate) const DEFAULT_SUBAGENT_TIMEOUT: Duration = Duration::from_secs(900);
