use std::time::Duration;

pub(crate) const FIXED_MODEL: &str = "gpt-5.6-sol";
pub(crate) const ACCEPTED_MODEL_CONTENT_LIMIT: usize = 16 * 1024 * 1024;
pub(crate) const PROVIDER_CODE_LIMIT: usize = 256;
pub(crate) const PROVIDER_MESSAGE_LIMIT: usize = 4 * 1024;
pub(crate) const ARENA_EXECUTION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub(crate) const ARENA_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const FILE_MAX_BYTES: usize = 4 * 1024 * 1024;
#[allow(dead_code)]
pub(crate) const COLLECTION_MAX_ITEMS: usize = 10_000;
#[allow(dead_code)]
pub(crate) const COLLECTION_MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
#[allow(dead_code)]
pub(crate) const TOOL_CONCURRENCY_LIMIT: usize = 16;
