use std::time::Duration;

#[cfg(test)]
use std::time::Instant;

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
pub(crate) const SUBAGENT_TIMER_HEADROOM: Duration = Duration::from_millis(1);

#[cfg(test)]
pub(crate) fn latest_representable_instant() -> Instant {
    let base = Instant::now();
    let duration = |nanoseconds: u128| {
        Duration::new(
            u64::try_from(nanoseconds / 1_000_000_000).expect("duration seconds fit u64"),
            u32::try_from(nanoseconds % 1_000_000_000).expect("subsecond nanoseconds fit u32"),
        )
    };
    let mut lower = Duration::ZERO.as_nanos();
    let mut upper = Duration::MAX.as_nanos();
    while lower < upper {
        let midpoint = lower + (upper - lower).div_ceil(2);
        if base.checked_add(duration(midpoint)).is_some() {
            lower = midpoint;
        } else {
            upper = midpoint - 1;
        }
    }
    assert!(lower < Duration::MAX.as_nanos());
    let latest = base
        .checked_add(duration(lower))
        .expect("latest instant must be representable");
    assert!(latest.checked_add(Duration::from_nanos(1)).is_none());
    latest
}
