# Research: Kiki manual compaction compared with Pi

## Scope

This document compares manual compaction in Kiki Phase 2 at merge commit `a6c6e47` with the immutable compatibility baseline `@earendil-works/pi-coding-agent@0.85.1`. It covers `/compact`, preparation, summary generation, continuity, persistence, cancellation, accounting, and presentation.

Pi 0.87.1 is installed globally, but it is not the primary baseline here. Its current documentation describes later additions such as per-model compaction overrides, context-edit recovery, and materialised retained tails. Automatic threshold compaction and overflow recovery are mentioned only where their presence or absence changes manual behaviour.

Pi references below use paths relative to the unpacked `@earendil-works/pi-coding-agent@0.85.1` package.

## Summary

- The successful idle `/compact` path is closely aligned: both use the active model, the same 16,384 reserve and 20,000 retained-token defaults, an approximately four-characters-per-token cut, iterative summaries, split-turn handling, 80% and 50% output budgets, and summary-plus-retained-tail projection.
- Kiki deliberately extends Pi's summary contract with `## Arena State` and an Arena-continuity wrapper because the Python Arena checkpoint survives compaction unchanged.
- Kiki has a stricter foreground protocol. It acknowledges a request as accepted, busy, or disabled; retains one request behind a settling Run; and does not queue ordinary prompts during compaction. Pi aborts the current operation immediately and queues multiple ordinary prompts while compaction runs.
- Kiki has stronger durable outcome semantics. Once a provider attempt starts, Applied, Failed, and Cancelled outcomes are SQLite entries if settlement succeeds. Pi persists only successful compactions; failures and cancellations are transient events.
- Kiki's file continuity evidence is stronger than Pi 0.85.1's lexical inference. Kiki records successful, normalised driver-observed file effects and recomputes cumulative file sets from immutable execution evidence. Pi derives paths from assistant tool-call arguments and carries prior compaction details.
- Pi is broader: manual and automatic compaction share one provider-neutral implementation, extensions can cancel or replace compaction, and the TUI queues work behind it. Kiki Phase 2 has a provider-neutral model seam but only an OpenAI production adapter, no compaction hooks, and no automatic compaction.
- The largest behavioural differences are lifecycle, durability, file evidence, Arena continuity, and extensibility, rather than the core summary and retention algorithm.

## Findings

### Command and foreground lifecycle

Both expose `/compact [instructions]` and append the optional text as `Additional focus:`. Kiki parses the command without consuming the draft, sends a runtime command, freezes draft mutation until a reliable acknowledgement arrives, and consumes the draft only on `Accepted` ([`src/tui.rs:604`](../../src/tui.rs#L604), [`src/tui.rs:711`](../../src/tui.rs#L711), [`src/tui.rs:1491`](../../src/tui.rs#L1491)). Pi clears the editor before awaiting `session.compact()` (`dist/modes/interactive/interactive-mode.js:2465`).

Kiki admits at most one pending manual compaction. A request is `Busy` if one is pending or active, and `Disabled` if the runtime is read-only; otherwise it is `Accepted` ([`src/runtime.rs:1120`](../../src/runtime.rs#L1120)). When a Run is active, Kiki requests cancellation without creating correction intent, retains the compaction, and starts it only after the Run and any already-requested correction settle ([`src/runtime.rs:1137`](../../src/runtime.rs#L1137), [`src/runtime.rs:1497`](../../src/runtime.rs#L1497), [`src/runtime.rs:2070`](../../src/runtime.rs#L2070)). Escape can cancel this retained request without issuing another cancellation to the settling Run ([`src/runtime.rs:1778`](../../src/runtime.rs#L1778)).

Pi's manual entry point calls `await this.abort()` before preparation. That aborts retries, any existing compaction or branch summary, and the current agent operation, then waits for idle (`dist/core/agent-session.js:1222`, `dist/core/agent-session.js:1474`). A second `/compact` therefore cancels the first and starts a replacement rather than returning busy. Both systems abandon an interrupted Run rather than retrying it as part of manual compaction, but Kiki makes the settlement boundary explicit while Pi waits through the general abort path.

During active compaction, Pi leaves the editor active and queues any number of ordinary Enter and Alt+Enter messages as steering or follow-up work, then flushes them after `compaction_end`; extension commands execute immediately (`dist/modes/interactive/interactive-mode.js:2519`, `dist/modes/interactive/interactive-mode.js:3382`, `dist/modes/interactive/interactive-mode.js:3635`). Kiki leaves the draft editable but does not queue prompt submissions: transcript admission is non-idle and direct runtime submissions receive a busy rejection ([`src/transcript.rs:2395`](../../src/transcript.rs#L2395), [`src/runtime.rs:1083`](../../src/runtime.rs#L1083)).

Kiki reports a small session as the non-durable notice `Nothing to compact.` ([`src/tui.rs:1550`](../../src/tui.rs#L1550)). Pi throws either `Already compacted` or `Nothing to compact (session too small)` and exposes that through its error event (`dist/core/agent-session.js:1484`).

### Preparation and retained boundary

The retention policy is substantially the same. Both walk backwards, estimate roughly one token per four content units, retain whole context units until `keepRecentTokens`, never leave a result detached from the action that produced it, and return no preparation when no historical context can be removed ([`src/compaction.rs:121`](../../src/compaction.rs#L121), [`src/compaction.rs:353`](../../src/compaction.rs#L353); `dist/core/compaction/compaction.js:308`, `dist/core/compaction/compaction.js:517`).

The units differ with each harness's context model. Pi cuts among message entries. User, assistant, bash, custom, branch-summary, and compaction-summary messages are valid boundaries; tool results are not (`dist/core/compaction/compaction.js:227`, `dist/core/compaction/compaction.js:274`). Kiki cuts among immutable root-context entries: user requests, complete code-and-execution entries, and final messages. This makes code plus its execution feedback indivisible by construction ([`src/compaction.rs:343`](../../src/compaction.rs#L343), [`src/compaction.rs:494`](../../src/compaction.rs#L494)).

Both prefer actual provider usage as the context baseline and estimate later content. Pi uses the last valid assistant usage in the rebuilt session message list and estimates trailing messages (`dist/core/compaction/compaction.js:100`). Kiki uses the newest usable normal attempt on a completed execution or final message after the latest Applied compaction, then estimates later canonical context bytes ([`src/compaction.rs:289`](../../src/compaction.rs#L289)). Kiki's estimator counts UTF-8 bytes and opaque provider replay; Pi estimates serialised message text and treats each image as 4,800 characters ([`src/compaction.rs:494`](../../src/compaction.rs#L494); `dist/core/compaction/compaction.js:153`).

Both detect a boundary inside one oversized user turn, summarize the earlier complete history and the early part of that turn separately, and join them with `**Turn Context (split turn):**` ([`src/compaction.rs:377`](../../src/compaction.rs#L377); `dist/core/compaction/compaction.js:609`). Both cap the normal summary at 80% of reserve tokens and the split prefix at 50%, bounded by the model output limit ([`src/compaction.rs:18`](../../src/compaction.rs#L18), [`src/compaction.rs:461`](../../src/compaction.rs#L461); `dist/core/compaction/compaction.js:488`, `dist/core/compaction/compaction.js:650`).

Repeated compaction also starts at the preceding kept boundary in both systems, so the previously retained tail can be folded into the next summary ([`src/compaction.rs:121`](../../src/compaction.rs#L121), [`src/compaction.rs:241`](../../src/compaction.rs#L241); `dist/core/compaction/compaction.js:517`). Kiki additionally validates that repeated boundaries move forward and remain context-bearing during restore ([`src/session.rs:3365`](../../src/session.rs#L3365)).

There are two focused split-turn differences in 0.85.1. Kiki passes custom focus to both the history and split-prefix requests ([`src/compaction.rs:149`](../../src/compaction.rs#L149), [`src/compaction.rs:449`](../../src/compaction.rs#L449)); Pi passes it only to the history request because `generateTurnPrefixSummary` has no custom-instructions parameter (`dist/core/compaction/compaction.js:618`, `dist/core/compaction/compaction.js:650`). If a repeated compaction has only a split prefix, Kiki reuses the previous summary without a provider call, or updates it through an empty-conversation request when focus was supplied ([`src/compaction.rs:149`](../../src/compaction.rs#L149)). Pi 0.85.1 substitutes `No prior history.` whenever `messagesToSummarize` is empty, even if `previousSummary` exists (`dist/core/compaction/compaction.js:614`).

### Summary prompts and provider calls

Kiki's system prompt and Pi 0.85.1's system prompt are textually equivalent. The initial and update formats preserve Pi's Goal, Constraints & Preferences, Progress, Key Decisions, Next Steps, and Critical Context sections. Kiki adds `## Arena State` to initial, update, and split-prefix prompts to carry model-authored descriptions of persistent Python values ([`prompts/compaction-system.md`](../../prompts/compaction-system.md), [`prompts/compaction-initial.md`](../../prompts/compaction-initial.md), [`prompts/compaction-update.md`](../../prompts/compaction-update.md), [`prompts/compaction-split-run.md`](../../prompts/compaction-split-run.md); `dist/core/compaction/compaction.js:352`).

The context wrapper is also intentionally different. Pi sends `The conversation history before this point was compacted into the following summary:` followed by `<summary>`. Kiki inserts an additional paragraph stating that the Python Arena persisted unchanged and that unverified Arena details must be inspected before use ([`src/model.rs:253`](../../src/model.rs#L253); `dist/core/messages.js:7`).

Both flatten historical context before summarisation rather than presenting it as a live conversation. Pi uses user, assistant thinking, assistant text/tool-call, and tool-result labels and truncates tool results to 2,000 JavaScript string characters (`dist/core/compaction/utils.js:78`, `dist/core/compaction/utils.js:94`). Kiki serialises its own canonical context as user, assistant reasoning/action, Python source, and execution-result blocks and truncates feedback to 2,000 Unicode scalars ([`src/compaction.rs:551`](../../src/compaction.rs#L551), [`src/compaction.rs:599`](../../src/compaction.rs#L599)).

Pi's built-in path is provider-neutral through `pi-ai`, uses the current model and thinking level, supports configured retries, disables prompt-cache writes, and allows an extension to replace the model or the result (`dist/core/compaction/compaction.js:454`, `dist/core/agent-session.js:1452`, `dist/core/agent-session.js:1496`). Kiki's `Model::summarise` interface and `SummaryRequest` are provider-neutral, but the production model is currently `OpenAiModel` only ([`src/model.rs:39`](../../src/model.rs#L39), [`src/model.rs:497`](../../src/model.rs#L497), [`src/main.rs:46`](../../src/main.rs#L46)). Its OpenAI request omits tools, streams with `store: false`, and carries the selected service tier and reasoning configuration ([`src/openai.rs:763`](../../src/openai.rs#L763)).

Kiki makes at most three stream attempts with a fixed 100 ms restart delay for restartable transport failures and records every attempt ([`src/openai.rs:32`](../../src/openai.rs#L32), [`src/openai.rs:792`](../../src/openai.rs#L792)). Pi delegates summarisation retries to its configurable retry policy (`dist/core/compaction/compaction.js:454`, `dist/core/agent-session.js:1452`). Kiki also applies stricter OpenAI response-shape checks: one completed assistant message, one non-whitespace text part, no functions, and only recognised reasoning items ([`src/openai.rs:1282`](../../src/openai.rs#L1282)). Pi rejects provider errors, length stops, and tool calls, then extracts text content (`dist/core/compaction/compaction.js:436`, `dist/core/compaction/compaction.js:488`).

### File continuity

Both append sorted `<read-files>` and `<modified-files>` sections and make modification dominate read-only status ([`src/compaction.rs:397`](../../src/compaction.rs#L397), [`src/compaction.rs:698`](../../src/compaction.rs#L698); `dist/core/compaction/utils.js:49`, `dist/core/compaction/utils.js:59`).

The evidence source differs. Pi 0.85.1 scans assistant tool-call arguments named `path` for `read`, `write`, and `edit`, without consulting the matching tool result, and merges prior Pi-generated compaction `details` (`dist/core/compaction/utils.js:15`, `dist/core/compaction/compaction.js:15`). Kiki records paths only from successful driver-observed `ToolEffect::Read`, `Write`, and `Edit` results, after workspace path normalisation, and persists them with immutable execution evidence ([`src/tools.rs:239`](../../src/tools.rs#L239), [`src/arena.rs:318`](../../src/arena.rs#L318)). It recomputes cumulative sets from selected root ancestry through the newly summarised range rather than trusting prior summary tags ([`src/compaction.rs:397`](../../src/compaction.rs#L397)).

Kiki strips model-produced reserved blocks before deterministic regeneration, rejects malformed, nested, inline, mismatched, or unmatched reserved tags, JSON-encodes each path, and escapes angle brackets ([`src/compaction.rs:636`](../../src/compaction.rs#L636), [`src/compaction.rs:715`](../../src/compaction.rs#L715)). Pi joins raw path strings into tags and stores the same lists in the successful compaction's `details` (`dist/core/compaction/utils.js:59`, `dist/core/compaction/compaction.js:635`). Kiki's raw observed-path fields stay out of ordinary execution cards and tree rows; they become user-visible only when the generated compaction summary itself is expanded or copied.

### Persistence, context projection, and branching

Both append a new tree node and preserve raw history. On a branch containing a successful compaction, model context becomes the summary followed by context from the retained boundary; navigating to an earlier branch without that compaction restores the older path ([`src/session.rs:3632`](../../src/session.rs#L3632); `dist/core/session-manager.js:198`). Both account summary generation separately from ordinary model turns.

Pi persists only a successful `CompactionEntry` to JSONL, containing `summary`, `firstKeptEntryId`, `tokensBefore`, optional usage, and details (`dist/core/session-manager.js:818`). Cancellation, provider failure, invalid preparation, and extension cancellation emit lifecycle events but append no node (`dist/core/agent-session.js:1539`, `dist/core/agent-session.js:1575`).

Kiki persists three outcome variants: Applied with summary, retained boundary, pre-compaction context measurement and attempts; Failed with bounded diagnostic and attempts; and Cancelled with attempts ([`src/session.rs:139`](../../src/session.rs#L139), [`src/session/codec.rs:92`](../../src/session/codec.rs#L92)). A no-op, cancellation before any provider attempt, or failure before any provider attempt remains non-durable. Once an attempt starts, Kiki selects an outcome and attempts settlement even after cancellation ([`src/agent.rs:617`](../../src/agent.rs#L617)). Failed and Cancelled nodes advance the tree leaf but clone the existing Model Context unchanged ([`src/session.rs:965`](../../src/session.rs#L965)).

Kiki settlement uses an immediate SQLite transaction, verifies that Active Leaf, parent Run, and Arena checkpoint still match preparation, inserts the compaction under that leaf, and advances the leaf atomically ([`src/session.rs:1742`](../../src/session.rs#L1742)). It reuses the parent entry's exact checkpoint rather than creating or restoring a new Arena checkpoint. Pi's JSONL append advances its in-memory leaf and appends the success entry, without an equivalent Arena or guarded database transaction (`dist/core/session-manager.js:760`, `dist/core/session-manager.js:818`).

Kiki's projection chooses the latest Applied outcome, ignores later Failed and Cancelled outcomes for Model Context, and validates retained-boundary ancestry on restore ([`src/session.rs:3632`](../../src/session.rs#L3632)). All three outcome types remain visible and branchable in Kiki's full and conversation trees ([`src/tui/tree.rs:1172`](../../src/tui/tree.rs#L1172)). Pi's tree contains successful compaction nodes only.

### Cancellation, presentation, and accounting

Pi emits `compaction_start` before model/authentication and preparation checks, displays a generic compacting spinner, binds Escape to abort, and emits `compaction_end` for success, cancellation, or failure (`dist/core/agent-session.js:1474`; `dist/modes/interactive/interactive-mode.js:2741`). Kiki enters a pending state on acceptance but emits `Started` only from the callback immediately before the first real provider attempt ([`src/agent.rs:636`](../../src/agent.rs#L636), [`src/runtime.rs:1955`](../../src/runtime.rs#L1955)). Its dedicated nine-cell, 50 ms convergence throbber is active only in that provider-active interval ([`src/tui.rs:60`](../../src/tui.rs#L60), [`src/tui.rs:1597`](../../src/tui.rs#L1597)).

On success, Pi rebuilds the visible chat, shows a collapsible compaction summary, and can show a compaction cost notice (`dist/modes/interactive/interactive-mode.js:2754`). Kiki appends a durable collapsed card for every persisted outcome. Applied cards expand to Markdown; Failed cards expand to the diagnostic; Cancelled cards expand to cancellation text; all carry compaction attempt accounting ([`src/transcript.rs:3626`](../../src/transcript.rs#L3626)).

Pi stores the successful summary call's combined usage and includes it in session totals (`dist/core/compaction/compaction.js:609`, `dist/core/agent-session.js:2664`). Kiki stores a list of attempt-accounting records on every durable outcome, including unavailable-usage markers for failed/restarted attempts, and folds those attempts into transcript/session totals without replacing the ordinary context measurement ([`src/session/codec.rs:92`](../../src/session/codec.rs#L92), [`src/transcript.rs:3626`](../../src/transcript.rs#L3626)).

### Settings, extensions, and automatic behaviour

The numeric defaults match. Kiki layers positive TOML `reserve_tokens` and `keep_recent_tokens` values and rejects every other `[compaction]` key, including `enabled` ([`src/config/settings.rs:832`](../../src/config/settings.rs#L832)). Pi 0.85.1 reads JSON `enabled`, `reserveTokens`, and `keepRecentTokens`, defaulting to true, 16,384, and 20,000; disabling automatic compaction does not disable `/compact` (`dist/core/settings-manager.js:547`; `docs/compaction.md:418`).

Pi exposes `session_before_compact`, success, and failure extension events. A hook may cancel compaction or provide an entirely custom summary, usage record, retained boundary, and details; extensions can also trigger compaction programmatically (`dist/core/agent-session.js:1496`; `examples/extensions/custom-compaction.ts`; `examples/extensions/trigger-compact.ts`). Kiki Phase 2 has no equivalent extension seam.

Pi 0.85.1 uses the same machinery for manual, threshold, and overflow compaction. Kiki Phase 2 exposes manual compaction only: there are no threshold checks, overflow compact-and-retry, or `enabled` behaviour. The `AutomaticThreshold` and `ContextOverflow` reason values exist in the durable vocabulary for later phases but are not production triggers in Phase 2 ([`src/transcript.rs:3659`](../../src/transcript.rs#L3659), [`README.md:58`](../../README.md#L58)).

## Key references

### Kiki

- [`src/agent.rs:617`](../../src/agent.rs#L617) — manual provider orchestration and durable settlement decision.
- [`src/compaction.rs:121`](../../src/compaction.rs#L121) — deterministic preparation, repetition, split requests, and file derivation.
- [`src/runtime.rs:1120`](../../src/runtime.rs#L1120) — accepted, busy, and disabled command protocol.
- [`src/session.rs:943`](../../src/session.rs#L943) — prepared snapshot and settlement publication.
- [`src/session.rs:1742`](../../src/session.rs#L1742) — guarded SQLite transaction.
- [`src/session.rs:3632`](../../src/session.rs#L3632) — latest-Applied Model Context projection.
- [`src/openai.rs:763`](../../src/openai.rs#L763) — production summary request.
- [`src/transcript.rs:3626`](../../src/transcript.rs#L3626) — durable outcome cards and accounting.

### Pi 0.85.1 package

- `dist/core/agent-session.js:1474` — manual lifecycle, hooks, append, cancellation, and errors.
- `dist/core/compaction/compaction.js:308` — cut-point selection.
- `dist/core/compaction/compaction.js:488` — summary prompt and provider options.
- `dist/core/compaction/compaction.js:517` — repeated-compaction preparation.
- `dist/core/compaction/compaction.js:609` — split-turn assembly and usage.
- `dist/core/compaction/utils.js:15` — lexical file-operation extraction.
- `dist/core/session-manager.js:198` — compaction-aware context projection.
- `dist/core/session-manager.js:818` — JSONL success entry.
- `dist/modes/interactive/interactive-mode.js:2465` — `/compact` command handling.
- `dist/modes/interactive/interactive-mode.js:2741` — TUI lifecycle presentation.
- `dist/modes/interactive/interactive-mode.js:3635` — queueing prompts during compaction.
- [`Compaction & Branch Summarization`](file:///tmp/pi-coding-agent-0.85.1/package/docs/compaction.md) — package-level behaviour and extension contract.
