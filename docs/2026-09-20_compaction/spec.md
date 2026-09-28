# Spec: Model context compaction

## Context

Phases 1 and 2 are merged: the Session Model owns the selected branch's live and restored Model Context, and `/compact` persists outcomes without rewriting Session Entries or Arena Checkpoints (`src/session.rs:987`, `src/agent.rs:617`). Automatic compaction and overflow recovery are still absent. A long selected context can exceed the active model's window, especially when a new request is appended or a session changes provider.

This change adds Pi-compatible Model Context compaction. It replaces an older prefix of the selected branch with a durable summary while retaining the exact recent tail. It does not delete or rewrite Session Entries, Display History, or Arena Checkpoints. The Python Arena and external filesystem effects therefore survive unchanged.

Kiki also has continuity data which Pi does not: useful persistent Python bindings and structured filesystem effects. Compaction summaries include an evidence-based `Arena State` section and cumulative successful `read`, `write`, and `edit` paths so the next model can reuse both forms of state.

## Goals

- Make the Session Model the single owner of live and restored Model Context projection; remove independently mutable context ownership from `Agent`.
- Automatically compact just before a normal root Model Step when the selected context, including a newly committed request, is estimated above the configured threshold; keep `/compact [instructions]` available below it.
- Recover once from a recognised provider context overflow by compacting and retrying the same pending Model Step, with provider-specific classification for OpenAI and Anthropic.
- Preserve the delivered Pi-inspired summary, retained-tail, repeated-compaction, split-Run, output-budget, and cumulative read/modified-file behaviour, while making estimates and summary input consistent with the selected provider's context rendering.
- Persist every provider-started compaction outcome while the process remains alive through settlement and storage and Session invariants remain valid; only an applied outcome changes Model Context.
- Preserve cancellation, usage accounting, full Display History, branch navigation, and inspectability through collapsed transcript cards and durable session-tree rows.

## Non-goals

- Branch summarisation, branch merging, history deletion, or pruning stored Session Entries and Arena Checkpoints.
- A general message/command queue; Runtime retains at most one pending manual compaction, including across a pending correction.
- A generic session-storage port, alternate persistence backend, migration framework, or compatibility with pre-compaction format-version-1 sessions.
- Moving Ratatui rendering or terminal layout into the Session Model.
- A new per-Model-Step Runtime state machine or an automatic foreground operation without a Run; the existing Agent loop owns pre-step decisions and generation.
- Pi's pre-prompt or post-final automatic trigger timing; Kiki checks only when a normal root Model Step is about to start.
- Automatic compaction or overflow recovery for task-local subagent histories; only the root Session Model has durable Compaction Entries.
- Configurable summary models, thinking levels, retry counts, estimators, prompts, or split-Run policy.
- Authoritative Arena variable enumeration, Monty introspection changes, or claims that the evidence-based `Arena State` section is complete.
- Exact provider tokenisation, a provider token-count request before generation, proactive summary chunking, truncated-output recovery, or a hard retained-tail token limit.
- New expansion controls or a compaction-specific viewer; transcript cards reuse global `Ctrl-O`, while the existing tree uses `Ctrl-X` copy.

## Key discoveries

- `SessionWriter::prepare_compaction` reads selected ancestry and `settle_compaction` atomically advances the Active Leaf and its projected Model Context only after a guarded append (`src/session.rs:987`, `src/session.rs:1009`). Existing manual compaction already owns summary generation and durable outcome selection (`src/agent.rs:617`).
- `RuntimeController::start_run` captures one `ModelSelection`; `Agent::run_persisted` uses it for every Model Step (`src/runtime.rs:1590`, `src/agent.rs:704`). Settings accepted mid-Run take effect on the next Run, not a later step or retry.
- The persisted Agent commits a request before allocating the first Model Step, and settles each successful execution before looping to allocate the next (`src/agent.rs:704`, `src/agent.rs:742`, `src/agent.rs:940`). These are the two instances of one pre-step seam.
- `ProviderModel` dispatches both steps and standalone summaries by the captured selection to OpenAI or Anthropic (`src/provider.rs:66`). Both adapters send the canonical compaction summary; neither uses a provider token-count call.
- `ProviderFailure` retains bounded status, code, and message without context-overflow meaning (`src/model.rs:407`). Both adapters see raw diagnostics before redacting and bounding them (`src/openai.rs:577`, `src/anthropic.rs:644`).
- `src/compaction.rs:293` can use the latest normal call's token usage even when its model differs from the next selection. Its fallback counts opaque replay bytes (`src/compaction.rs:510`), while a foreign provider renders semantic parts and can omit reasoning under `cross_provider_reasoning = "drop"` (`src/model.rs:253`). Summary input still serialises that reasoning unconditionally (`src/compaction.rs:559`).
- Anthropic's documented thinking-block binding rejects replayed signed thinking when its earlier conversation prefix changes; compaction can change that prefix while retaining native Anthropic entries. The documented `drop_block` behaviour with its beta header lets the adapter keep valid signatures and drop mismatched ones ([Claude API errors](https://platform.claude.com/docs/en/api/errors#thinking-block-no-longer-matches-the-conversation); `src/anthropic.rs:99`).
- The compaction snapshot currently loads all Session Entries and provider replay blobs before isolating selected ancestry (`src/session.rs:2422`). For each Compaction Entry, fact decoding rebuilds the payload list and ancestor validation scans it linearly (`src/session.rs:2589`, `src/session.rs:3422`). Per-step checks must avoid both abandoned replay reads and repeated full-chain validation scans.
- `codec::decoded_attempts` currently serves both context measurement and off-branch totals (`src/session.rs:2553`, `src/session.rs:2994`); initial overflow attempts must contribute to billing without becoming a normal context-usage baseline or the footer's latest normal usage (`src/transcript.rs:3024`).
- `Agent::compact_manual` already owns the summary calls, attempt collection, provider-started signal, cancellation choice and settlement (`src/agent.rs:617`). The persisted Run has seven terminal closure sites (`src/agent.rs:732–1032`); recovery evidence needs one Run-long owner and consistent terminal attachment.
- Phase 2 already persists file effects, immutable Compaction Entries, transcript/tree projections, and manual non-Run foreground lifecycle; Phase 3 extends those delivered seams rather than creating a second context owner (`src/session.rs:987`, `src/runtime.rs:1532`).

## Proposed approach

### Canonical Session Model

The Session Model is the canonical live and durable Model Context owner. It maintains the selected branch projection and supplies immutable snapshots to provider calls. A successful Session Entry commit updates that projection; failed storage leaves it unchanged. Restoration and live operation use the same projection rules.

Runtime remains responsible for provider validation and Arena restoration. After the target history and checkpoint validate, one guarded Session activation operation persists navigation selection when needed and publishes the matching immutable history only after commit. Startup activates the already selected projection through the same seam. Failed validation, Arena restoration, stale selection, or persistence leaves both the prior Active Leaf and live history unchanged.

Agent does not own mutable root Model Context. It owns the selected model and Arena, and orchestrates generation and Python execution. The persisted root loop reads and commits context through the Session Model. Ephemeral subagent runs keep task-local history outside Agent.

Runtime moves Agent into the foreground worker and passes a cloned Session Model handle, retaining its own shared handle in the controller. Only Agent returns at settlement. Runtime records remain a Display History cache, not a Model Context authority. Automatic threshold and overflow work stay inside this Run worker; manual compaction retains its existing non-Run foreground worker.

### Provider-neutral model operations

The provider-neutral `Model` contract already supports standalone summarisation. A summary request contains Kiki-owned instructions, serialised conversation text, the captured model selection, and an output limit. A generated summary returns text and attempt accounting without provider replay. `ProviderModel` routes both normal steps and summaries to OpenAI or Anthropic through that selection; neither Session nor Runtime parses their wire formats. Incomplete, empty, or tool-call summaries fail the compaction. One internal Agent summary operation owns the request loop, provider-started signal, attempt collection, cancellation choice and guarded settlement for manual, threshold and overflow reasons. Runtime retains manual command admission and acknowledgements; the containing Run interprets automatic outcomes. Do not duplicate summary orchestration or introduce a generic Compactor port.

A provider-neutral compaction-summary Model Context entry owns its canonical text:

```text
The conversation history before this point was compacted into the following summary.

The Python Arena and its existing variables, functions, classes, and data persisted unchanged across compaction. Treat Arena details marked unverified as historical evidence and inspect them directly before relying on them.

<summary>
{{summary}}
</summary>
```

Estimation counts this same text. Provider adapters only encode it into their wire format, preventing wrapper drift. New providers use the same Model operations without exposing wire formats to the Session Model.

Anthropic can retain signed thinking in an assistant replay after a compaction changes its preceding conversation. For the supported Anthropic models, the normal-step adapter must request the documented `thinking.block_binding.prefix_mismatch_behavior = "drop_block"` with its required beta header: unchanged signatures remain usable, mismatched blocks are dropped by Anthropic rather than failing the next request. Keep durable replay and the exact retained tail unchanged; summarisation requests have no replayed thinking. Verify request construction and the retained-tail continuation, not just an empty-history summary.

### Provider-specific context-overflow classification

The provider-neutral failure vocabulary exposes whether a normal Model Step failed from recognised context overflow. Each provider adapter classifies its own raw diagnostic before redaction/bounding and stores the semantic result beside bounded diagnostics. The OpenAI adapter classifies overflow as follows:

- An ASCII case-insensitive exact error code of `context_length_exceeded` or `model_context_window_exceeded` is recognised unless HTTP status is 401 or 429.
- Without one of those codes, only HTTP 400 or 413 is eligible. Its ASCII case-insensitive message must contain `exceeds the context window`, `maximum context length`, `context length exceeded`, or `context window exceeded`.
- Generic HTTP 400/413, authentication, rate-limit, `too many tokens`, and unrelated `maximum` messages remain ordinary provider failures.

Code matching takes precedence over message matching subject to the 401/429 exclusion, including when the exact code accompanies an otherwise ineligible status such as 500.

Anthropic's `invalid_request_error` covers unrelated malformed requests, so it is never enough on its own. Recognise only an HTTP 400 `invalid_request_error` with the provider's specific `prompt is too long: <input> tokens > <maximum> maximum` diagnostic, after verifying that shape against a current provider response. A generic HTTP 400/413, `request_too_large` (a request-byte limit), rate limit, overload, `max_tokens` output stop, or a signature-binding error is not context overflow. If the positive signature cannot be verified, leave Anthropic overflow recovery unsupported rather than widening the matcher; automatic thresholds and manual compaction must still work on Anthropic. Neither adapter classifies local response-size limits or transport failures as context overflow. Recovery consults the semantic result only for normal Model Steps; a similarly classified summary failure never recursively compacts.

After recovery starts, composable terminal evidence preserves one initial overflow observation alongside ordinary retry result/evidence. The initial observation owns its bounded structured failure, optional partial generation, and failed pre-compaction normal attempts; immediate overflow before any chunk stores no generation evidence. Retry code/final content or ordinary terminal generation/execution evidence remains the sole retry representation, and retry attempts remain exclusively on that code, Final, Failed, or Cancelled terminal's ordinary attempt field. Session totals include both attempt owners once on selected and abandoned branches, but the ordinary-attempt view used for context measurement excludes initial overflow attempts; the billable view includes them. The footer includes both in costs without using initial overflow usage as its latest normal context measurement. Agent keeps one Run-long initial observation and attaches it through one terminal-sealing path after the existing late-cancellation choice, covering Final, Failed and Cancelled even after later successful code. The final Run message reflects the last surrounding failure. Capture partial generation in the initial observation and clear Agent's per-generation evidence before retry, even if the retry never emits an `AttemptStarted` event or any text. Before retry, a reliable generation-epoch reset separates its live progress from delayed initial chunks while keeping the same durable Model Step. Tag lossy generation progress by epoch and make delayed same-step `ModelStepStarted` harmless so old chunks or start events already forwarded through Runtime or buffered by the TUI cannot repaint the retry or interrupt compaction status; retain settled usage. A process crash can lose in-memory evidence/attempts; existing generic interruption recovery uses durable rows only and does not invent them.

### Configuration and triggers

Add file-based user/project settings:

```toml
[compaction]
enabled = true
reserve_tokens = 16384
keep_recent_tokens = 20000
```

`enabled` defaults to `true` when omitted, including for existing Phase 2 numeric-only settings. Fields use strict validation and field-wise project precedence. Numeric values must be positive and safely representable. There are no environment, session-preference, runtime-picker, or `Ctrl-S` overrides.

Automatic compaction uses the strict threshold:

```text
context tokens > model context window - reserve tokens
```

The subtraction saturates at zero when a configured reserve is at least the selected model's window. When enabled, the persisted root Agent checks immediately before each normal Model Step, after the user request or prior execution has committed but before allocating the step. The first check includes the new request in the estimate, while the request remains an ordinary exact retained-tail entry; a later check includes settled execution feedback. No automatic compaction runs before Run admission or after a final response. When nothing can be removed, skip silently and attempt the scheduled step; a non-cancellation provider-started threshold failure persists Failed and also proceeds with unchanged Model Context. The threshold is an estimate with headroom, not a guarantee that the next provider request fits.

`/compact [instructions]` forces preparation below the threshold and remains available when automatic compaction is disabled. Each command receives an immediate Accepted, Busy, or Disabled acknowledgement before later lifecycle events; every acknowledgement, lifecycle transition, and durable card uses the same reliable FIFO event channel rather than lossy progress. The TUI permits only one unacknowledged Compact dispatch and retains a rejected command's exact editor draft. If a foreground Run is active, Runtime retains one pending manual compaction, cancels and settles the Run, then compacts the resulting Active Leaf with a fresh cancellation token. Esc received before that retained operation starts clears it, emits a reliable no-card `Settled(CancelledBeforeStart)` lifecycle, and still lets the already-cancelled Run/correction finish; no later compaction starts. If early-Escape correction is already pending, the retained compaction waits for that correction outcome and then targets whichever Active Leaf remains selected. Additional Submit or Compact commands are rejected busy; this is not a general queue. Background subagents continue because their child histories are isolated until explicitly retrieved.

The Run captures one `ModelSelection` at admission. Every normal step, threshold decision, summary budget, overflow compaction and retry in that Run uses it; changes accepted mid-Run apply to the next Run. An idle or retained manual compaction instead captures the then-accepted selection when it starts. A recognised provider context overflow compacts and retries the same pending Model Step once. A second overflow, a failed overflow compaction, or any different retry failure settles the Run normally as failed. `enabled = false` disables both threshold compaction and overflow recovery; manual compaction remains available.

### Deterministic context measurement

Measure the committed selected Model Context immediately before the normal Model Step. Prefer provider usage only for a response under the captured Run's exact model, after the latest applied compaction, when all ordinary replay in the current projection is native to that provider. Search projected `CompletedCode` and `FinalMessage` entries newest-first for usable ordinary attempts; if the newest known response usage belongs to a different model, or any projected replay is foreign, use the full fallback instead of an older baseline. Mixed-provider rendering can change under `cross_provider_reasoning` on resume without changing stored messages, so its previous usage is not a stable baseline. Retained ancestors of an Applied entry are ineligible because their usage describes the pre-compaction prefix. Terminal code is eligible only when it projects as completed code; failed/cancelled terminals without projected code, summary attempts, and initial overflow attempts are never eligible. `KnownUsage` and `KnownUsageWithUntrackedRetries` can contribute when these conditions hold:

1. Use non-zero `total_tokens` when present.
2. Otherwise require both `input_tokens` and `output_tokens` and use their checked, non-zero sum.
3. Both adapters normalise cached input into Kiki's `input_tokens`; cache-read and cache-write fields are subsets, and reasoning output is a subset of `output_tokens`. Never add those details again.
4. Skip missing, zero, or overflowing candidates. With no eligible baseline, estimate the complete projected context.

A usage baseline represents context through that assistant response. For `CompletedCode`, add its later execution feedback, not its assistant replay again; then estimate later projected segments. On the full fallback, count each projected entry for the captured provider's rendering and resolved `cross_provider_reasoning` policy:

- `UserRequest`: UTF-8 bytes of its text, including the request just committed before the first Model Step.
- Compaction summary: UTF-8 bytes of the complete fixed wrapper above plus its stored summary.
- Native-provider `CompletedCode` and `FinalMessage`: raw bytes of opaque assistant replay items; add execution feedback once for completed code.
- Foreign-provider `CompletedCode` and `FinalMessage`: UTF-8 bytes of the values returned by `ModelContextEntry::foreign_parts`, which both adapters already consume, not discarded opaque replay or provider JSON syntax. Count each included reasoning, commentary and final-text part; for a tool call count its call ID, literal tool name `execute_python`, source and execution feedback once each. Apply `drop` to foreign reasoning summaries. Do not add execution feedback again outside the tool-call part.
- Failed or cancelled terminal records without completed code: zero because they do not enter Model Context.

Sum units with saturating arithmetic and divide by four, rounding up. Full fallback, trailing additions, and retained-tail selection use one deterministic approximate recipe. Neither a prior provider's usage nor opaque foreign replay may masquerade as the next provider's exact token count. The estimate is for Kiki's selected Model Context, not an exact count of system instructions, tool schema, wire overhead, or future output; reserve headroom and bounded overflow recovery are still necessary.

### Retained tail and split Runs

Starting at the Active Leaf, retain whole ordinary context-bearing entries backwards until approximately `keep_recent_tokens` have been selected. Never split a `CompletedCode` entry from its result. The tail is approximate: one indivisible entry can exceed the target.

The latest applied compaction supplies the previous summary and previous retained boundary. Its summary contributes to full-context threshold measurement, but the applied Compaction Entry is never a retained-tail cut point or serialised conversation entry. Tail selection scans only ordinary projected entries at or after the previous retained boundary, so the old summary does not consume `keep_recent_tokens`; the new summary replaces it through `<previous-summary>`. Failed and cancelled Compaction Entries are also ignored as cut points and conversation content.

Repeated compaction never revisits ordinary entries before the previous retained boundary. It requires the new cut to move past at least one newly discarded ordinary entry while retaining at least one valid ordinary entry. If no such boundary exists, preparation is the specified no-op even when the previous summary alone is large. Manual focus updates a previous summary only when compaction otherwise has removable content.

Find a split-Run start by searching backwards from the first retained entry only as far as the current summarisation boundary: Session root initially, or the previous retained boundary after compaction.

- If the first retained entry starts a Run, the history range ends immediately before it and no split request is needed.
- If a user request exists inside that bounded search range, the history range ends before that request and the split-prefix range runs from the request through the entry before the retained tail.
- If no user request exists because the previous boundary already falls inside the same Run, do not split again. Feed all newly discarded entries from the previous boundary through the entry before the retained tail to the repeated-history update.
- The exact retained tail always starts at the first retained entry.

These ranges neither overlap nor leave a gap. The history request receives the previous summary when present. Manual `Additional focus: …` instructions apply to both history and split-prefix requests, intentionally extending Pi so prefix-only compaction cannot ignore the user's focus. If a prefix-only split has a previous summary and manual focus, issue an empty-conversation update request before the split-prefix request; without focus the unchanged previous summary may be reused. With neither previous summary nor history input, use literal `No prior history.`.

Combine history and prefix summaries with Pi's separator and literal `Turn Context (split turn)` wording. Normal summary output is capped at the lower of the selected model maximum and 80% of `reserve_tokens`; a split-prefix summary uses 50%. Both derived budgets have a one-token minimum.

Manual compaction is a no-op with a clear notice when no structurally valid prefix can be removed. The same condition makes automatic preparation skip silently and makes overflow recovery fail with the original overflow plus a bounded diagnostic.

### Prompt resources and summary input

Store Pi's system, initial-summary, repeated-compaction, and split-turn-prefix prompts as Markdown resources beside `prompts/system.md` and `prompts/subagent.md`. Preserve Pi's text except for this additional section in every output format which summarises discarded conversation, including the split-prefix format:

```markdown
## Arena State
- `<name>` (`<latest known type>`) — <purpose and useful contents>
- Include useful persistent variables, functions, classes, and datasets.
- Mark uncertain availability or type as unverified.
- Do not claim this is a complete live-variable inventory.
- State "(none identified)" if no useful Arena state is evidenced.
```

Serialise the selected prefix with explicit user, assistant reasoning, assistant action, and execution-result labels. For a foreign assistant entry, omit its reasoning summary from new summary inputs when the selected summary provider's resolved `cross_provider_reasoning` is `drop`, as normal cross-provider replay does; leave same-provider reasoning and durable entries unchanged. This does not redact content already included in a stored earlier summary. Phase 2's implementation plan pins the other byte-level mappings, separators, wrappers, empty optional fields, split-prefix fallback, and focus suffix. Truncate each execution result to Pi's 2,000-Unicode-scalar summarisation limit without altering durable history. A repeated summary is supplied inside `<previous-summary>` tags. Conversation text is untrusted material to summarise, not an instruction source; fixed system and summary prompts retain authority.

### Arena and file continuity

Compaction reuses the prepared Active Leaf's exact Arena Checkpoint. It never creates, restores, probes, or mutates the Arena.

Record normalised workspace-relative paths for successful root-agent `read`, `write`, and `edit` results which the Arena driver observes and settles. Do not separately expose the new effect vectors through tool summaries, ordinary cards, tree execution rows/search/copy, or execution feedback. Existing Python source and commentary remain unchanged and can independently contain lexical paths. Ordinary Python errors commit their resulting Python state and observed file effects. Cancellation discards tentative Python state by restoring the prior checkpoint, but its evidence still retains already-settled external file effects.

This is useful continuity metadata, not a complete filesystem audit. A tool can finish its external mutation while cancellation wins before the driver observes its result; that path is unknowable and may be absent. Existing Session Entries also have no paths to backfill. Compaction never infers missing operations from current checkpoint authorisations.

For each preparation, derive cumulative sets from immutable root-agent execution metadata on selected ancestry through the end of the newly summarised range. Deduplicate and sort paths. A path which was modified at any included point appears only in `<modified-files>`, never also in `<read-files>`. Both `write` and `edit` count as modified. Append only non-empty Pi-style sections to the stored Markdown summary; do not persist a duplicate cumulative-list authority on the Compaction Entry. Retained-tail operations are included only when that tail is absorbed by a later compaction. Child-agent file activity remains in child history and reaches root context only through explicitly retrieved child results.

The `Arena State` section remains model-authored evidence. Kiki does not parse checkpoints or claim that listed bindings still exist or retain the inferred type.

### Preparation and durable outcomes

Preparation is a read-only Session Model operation. Its SQLite read follows only the selected ancestry inside one consistent transaction rather than loading abandoned branches and their replay blobs; reuse indexed selected rows for ancestor validation instead of rebuilding and linearly rescanning the payload list per Compaction Entry. For an automatic threshold check, measure that selected context and return below threshold before building retained-tail or summary prompts. A triggered or manual preparation captures the expected Active Leaf, context measurement, summary ranges, first retained entry, previous summary, cumulative files, prompt inputs, and reason. No second context authority or SQLite transaction spanning model generation is introduced.

Subject to the process remaining alive through settlement and storage remaining writable, every provider-started compaction settles as an immutable branch-local Session Entry:

- **Applied**: final summary, first retained entry, context measurement, reason, and summary attempts.
- **Failed**: diagnostic bounded to the first 4,096 UTF-8 bytes at a valid scalar boundary with no truncation marker, reason, and summary attempts.
- **Cancelled**: reason and summary attempts.

Cancellation before a provider attempt starts writes nothing. Failed and cancelled entries remain inspectable and count towards session usage, but Model Context ignores them.

A normal commit verifies that the expected Active Leaf is still selected, reuses its Arena Checkpoint and Run provenance, appends the outcome, and advances the Active Leaf atomically. Threshold compaction happens after its request or preceding execution has committed and inherits that open Run's provenance; overflow compaction happens while its Model Step is pending. Compaction itself creates neither a synthetic Run nor a Model Step. Applied, Failed, and Cancelled outcomes are valid branch points: selecting one in `/tree` restores through that entry and a later request becomes its child. Applied changes the projected prefix; Failed and Cancelled retain their parent's Model Context.

The current single-foreground-operation design makes a stale prepared leaf unreachable. Keep the expected-leaf guard, but do not add speculative off-branch settlement machinery. A mismatch is an invariant/persistence failure: append nothing, leave the current projection unchanged, and disable further state-changing work through existing policy. Storage failure or process loss before append is the physically unavoidable exception to durable outcome recording. Compaction deliberately creates no separate in-progress Run, Model Step, or transaction journal, so restart recovery does not invent a lost manual, threshold, or overflow summary attempt. An interrupted containing Run and any pending Model Step use existing generic recovery without replaying work.

The latest applied compaction on selected ancestry controls projection:

```text
compaction summary
+ entries from the first retained entry through the compaction parent
+ context-bearing descendants after the compaction
```

All older Session Entries remain available to Display History and future tree navigation. Sibling branches continue to project their own ancestry. Summary attempts on selected or abandoned branches count once in session totals but never replace the latest normal context measurement.

Phase 2 advances the SQLite session format directly from version 1 to version 2 for the new durable entry vocabulary. Kiki is still in development, so version-1 sessions are rejected rather than migrated or opened through compatibility code. Phase 3 keeps version 2 and extends terminal payloads additively: existing Phase-2 `evidence` wire shapes remain decodable and a new optional overflow-recovery field is absent from old records.

### Cancellation and failure matrix

Esc cancels the current foreground token. After summary generation settles, orchestration checks cancellation immediately before choosing the immutable outcome passed to persistence. This check is the cancellation linearisation point:

- cancellation observed there chooses `Cancelled` when a provider attempt started;
- otherwise a complete valid summary chooses `Applied`, and a non-cancellation error chooses `Failed`;
- once chosen, the outcome wins even if Esc arrives before or during the awaited append transaction. Later Esc cannot rewrite or roll back it, but still cancels the surrounding Run or suppresses an overflow retry.

Cancellation observed by the linearisation point has these surrounding effects:

| Compaction point | Surrounding result |
| --- | --- |
| Manual compaction | Leave context unchanged and return idle. |
| Threshold before the first or a later Model Step | Settle the already admitted Run as cancelled; do not allocate/start another Model Step. |
| Overflow recovery | Settle the pending Run as cancelled while retaining overflow evidence and accounting. |

If Esc arrives after `Applied` is chosen, the compaction remains applied once the append succeeds. Manual work returns normally; a threshold Run settles cancelled without starting the next Model Step; overflow recovery does not issue its retry. A cancellation before a summary provider attempt writes no Compaction Entry but has the same surrounding result. A new request is already durable when the first threshold check starts, so its cancelled Run remains inspectable; there is no held-request preflight or post-final automatic delay. A manual compaction retained behind correction starts only after correction and `RunSettled`, always preparing against the resulting selected leaf, including when correction is skipped or fails without disabling the Runtime. Esc before `Started` cancels only that retained follow-up through `Settled(CancelledBeforeStart)` while correction/Run settlement continues. If Final or Failed wins the cancellation race and no correction event occurs, terminal publication and `RunSettled` still precede retained manual compaction from that terminal leaf. In either path, settlement which disables state-changing work clears the retained request. Non-cancellation manual failure returns idle. A non-cancellation summary failure before any provider attempt writes no Compaction Entry: threshold work proceeds with unchanged context; overflow recovery closes the Run with its bounded, safely redacted pre-start diagnostic. A provider-started threshold failure records Failed and continues. Failed, NoOp, or pre-start-failed overflow recovery closes the Run with this exact message: `context overflow: {original}\ncompaction recovery failed: {recovery}`. Each substituted component is independently truncated to its first 2,000 UTF-8 bytes at a scalar boundary with no marker, so the complete message is at most 4,047 bytes. `{original}` is the bounded/redacted provider failure display; `{recovery}` is the Failed Compaction diagnostic, the bounded/redacted pre-start diagnostic, or literal `compaction could not remove a valid prefix` for NoOp. A Failed Compaction Entry retains its complete separately bounded 4,096-byte diagnostic, while the Run message deliberately repeats only that 2,000-byte excerpt. Storage failure follows existing read-only/disabled policy.

Normal Model Steps, failed overflow attempts, and summary calls retain separate attempt accounting. Applied, failed, and cancelled compaction attempts contribute to session totals exactly once. Summary usage never replaces the latest normal context measurement shown in the footer.

### Presentation

While generating a summary, the status line reads `compacting context`; Esc remains the cancellation control. Automatic threshold/overflow summaries are foreground work inside the containing Run; they do not emit a manual `/compact` acknowledgement or consume its editor draft. Manual compaction keeps the existing foreground operation without a Run ID. Automatic work uses a reliable Run-scoped Started notification to drive both status and spinner, not the manual command's pending state; its committed card, terminal settlement or disabled event ends that presentation. While it is active, delayed normal-step visual progress and step-start events cannot change status or repaint the screen, even before an overflow retry reset; settled attempt accounting remains visible. Both make submission and tree navigation busy, and use reliable lifecycle/card events rather than lossy model progress.

Applied transcript entries render collapsed by default:

```text
╭compaction · automatic threshold
╰Compacted 1.03m tokens · Ctrl-O to expand
```

Manual and overflow entries identify their reason. In the transcript, `Ctrl-O` reveals the exact stored Markdown summary, including `Arena State`, cumulative file sections, and summary-call accounting. Failed and cancelled entries use the same compact card shape with bounded diagnostics. Full pre-compaction Display History remains visible and selectable.

`/tree` shows Applied, Failed, and Cancelled Compaction Entries in both full and conversation modes with a distinct semantic colour. Exact rows are `Compaction · {reason} · applied · {measurement} tokens`, `Compaction · {reason} · failed`, and `Compaction · {reason} · cancelled`; reason labels are `manual`, `automatic threshold`, and `context overflow`, and measurement uses the existing footer token formatter. Search canonicalises only the complete row text, not focus, summary, or diagnostics. The entry participates in latest-descendant branch ordering. `Ctrl-X` copies only the outcome payload: stored summary for Applied, bounded diagnostic for Failed, and exact reason label for Cancelled, each passed through the tree's existing terminal-control normalisation before reaching the clipboard. Tree rows remain single-line and do not use transcript `Ctrl-O` expansion.

## Key decisions & tradeoffs

- Session Model ownership over Agent ownership or a standalone Compactor: one authority serves live operation, restoration, and future branch selection.
- The existing provider-neutral summarisation operation and one Agent summary loop over separate per-trigger loops: adapters own wire formats and classify their own errors, while Agent keeps outcome/cancellation rules in one place without a new Compactor or provider registry.
- The Run's captured model and thinking selection for every step, summary and retry over a cheaper fixed summary model or mid-Run re-selection: no extra credentials, quality policy, or selection authority.
- Pi's reserve and retained-tail defaults over percentages: proven behaviour and predictable output headroom.
- One approximate byte-based estimator using the existing shared foreign parts over provider tokenisers or an extra counting call: deterministic estimates without new dependencies, with model-matched usage baselines and provider-visible foreign history instead of stale cross-provider counts.
- Selected-ancestry Session reads and separate ordinary/billable attempt views over a new context cache or single overloaded attempt accessor: keep hot-path cost and accounting invariants local to their existing owners.
- A just-in-time check before each Model Step over Pi's pre-prompt and post-final checks: the new request contributes to the estimate, final answers settle promptly, and cancelled first-step compaction retains the already committed request.
- The documented Anthropic `drop_block` binding control over rewriting signed retained replay or accepting persistent post-compaction 400s: unchanged signatures remain native and invalid ones are ignored by the provider.
- Immutable outcome entries over success-only persistence: failed and cancelled calls remain inspectable and durably accounted.
- An expected-leaf invariant guard over speculative off-branch settlement: current single-operation sequencing makes stale preparation unreachable.
- Parent Run provenance over a synthetic compaction Run: compaction is structural post-processing, not a user request or Model Step.
- Persist execution file operations over deriving paths from display summaries or checkpoints: normalised success-confirmed paths survive errors without separately exposing those vectors in ordinary cards or execution-tree copy.
- Modified-file precedence over duplicate read/modified tags: match Pi's cumulative file lists.
- Root selected-context file tracking over recursively scanning child histories: match current subagent isolation.
- Evidence-based `Arena State` over fake introspection or a Monty fork: useful continuity with explicit uncertainty.
- Apply manual focus to split-prefix requests over exact Pi focus routing: prevent prefix-only compaction from silently ignoring user instructions.
- One compact-and-retry over repeated recovery: bounded cost and no retry loop when compaction cannot make progress.
- Show every compaction outcome in conversation-mode tree history over hiding structural failures: match Kiki's retained Cancelled/Failed conversation rows and keep durable accounting visible.
- Retain one manual compaction through correction over rejecting or replacing correction: a narrow pending operation demonstrates ordered follow-up without adding a general message queue.
- Normalised workspace-relative paths over lexical tool arguments: use success-confirmed `ToolEffect` identity and collapse equivalent spellings.
- Format version 2 without migration over compatibility machinery: Kiki is still in development and a clean persisted-vocabulary boundary is simpler.
- Three vertical phases over one large PR: isolate context ownership, deliver complete manual compaction with continuity metadata, then add automatic triggers and recovery.

## Risks / unknowns

- Summary quality is probabilistic. A summary can omit an important decision or repeat misleading conversation content; immutable history and expansion preserve evidence, but future Model Context still depends on the generated checkpoint.
- `Arena State` can be stale, incomplete, or type-inaccurate. Its prompt and wrapper must keep that uncertainty visible.
- Approximate estimation can trigger early or late, especially after model changes and with large tool results. The first-step check includes the new request but still omits an exact provider tokenisation, system/tool overhead and future output. Overflow recovery covers only recognised late cases.
- Anthropic's precise positive token-overflow wording is not guaranteed by its published error type alone. Confirm the narrow `prompt is too long: ... tokens > ... maximum` signal against a current provider response before enabling its matcher; if unverified, treat it as an ordinary failure rather than confusing byte-size, signature, authentication or rate-limit errors with context overflow. The documented signed-thinking binding control also needs fixture coverage and an opt-in live compaction smoke test.
- Checking every Model Step still costs work proportional to the selected ancestry. Avoid abandoned replay reads, repeated full-chain validation scans and prompt assembly below threshold; measure remaining selected-branch cost before considering copy-on-write history or a separate usage cache.
- Provider overflow diagnostics can change. Narrow classifiers may need provider-specific updates rather than broad message guessing.
- Normalised workspace-relative file paths are sensitive local data. They already exist transiently in tool effects; this change additionally persists them and later exposes cumulative paths to the selected model through summaries and Applied tree copy.
- File continuity is best-effort. Cancellation can win after an external effect but before its result is observed, and legacy Session Entries contain no path metadata.
- A single indivisible retained entry can exceed both `keep_recent_tokens` and the provider window. The bounded retry then fails visibly; this change does not chunk one execution.
- Summary generation occurs outside the database transaction. The expected-leaf guard protects current invariants; future concurrency would need an explicit stale-attempt settlement design.
- Phase 2 intentionally rejects existing format-version-1 development sessions. There is no compatibility or migration path.

## Dependencies

- Existing domain and architectural direction: [CONTEXT.md](../../CONTEXT.md), [Vision](../VISION.md), and [Pi architectural lessons](../research/2026-08-25_pi-architectural-lessons.md).
- Existing persisted Session tree and immutable-entry rules: [Persisted session trees](../2026-09-15_session-trees/spec.md), its [interactive navigation plan](../2026-09-15_session-trees/phase-2_plan.md), and [tree UX plan](../2026-09-15_session-trees/tree-ux_plan.md).
- Current merge-impact analysis: [Session-tree merge impact on compaction](../research/2026-09-21_session-tree-compaction-impact.md).
- Existing Monty limitations and opaque checkpoint guarantees: [Monty feasibility](../research/2026-08-26_monty-feasibility.md).
- Immutable npm package `@earendil-works/pi-coding-agent@0.85.1` (integrity `sha512-FGRN+OHbWaefBPGaTggAdLjrIHW+s2PzLyglz/5dfLzb9of7uuXMXYC0fJIeZTw+shS32o2cuQ9jF7YSDuL/oQ==`) supplied the original summary/prompt baseline. Installed Pi 0.87.1 confirms the pre-prompt check excludes the incoming prompt (`dist/core/agent-session.js:1273`); Kiki intentionally diverges. Kiki has no runtime Pi dependency.
- [Anthropic API errors](https://platform.claude.com/docs/en/api/errors#thinking-block-no-longer-matches-the-conversation) document signed-thinking prefix binding and the `thinking-binding-controls-2026-08-01` beta; [request size limits](https://platform.claude.com/docs/en/api/errors#request-size-limits) distinguish HTTP 413 byte limits from context tokens. Pi 0.87.1's overflow matcher lists a candidate Anthropic prompt-too-long phrase (`pi-ai/dist/utils/overflow.js:9`), not a guarantee of current Anthropic wording.
- Existing OpenAI and Anthropic adapters, layered TOML settings, SQLite session storage, Runtime worker, transcript cards, and global expansion control. No new crate is required.

## Phase outline

Each phase is one reviewed PR. Later phases build on accepted behaviour and interfaces rather than carrying temporary alternatives.

1. **Canonical Session Model context (merged)** — Root Model Context ownership moved from Agent to the Session Model; live continuation, branch selection and restart use the committed projection.
2. **Manual compaction and continuity (merged)** — Format-v2 sessions, numeric compaction settings, summary resources, deterministic preparation, file-operation evidence, durable outcomes and `/compact [instructions]` are available. `enabled` remains invalid until Phase 3.
3. **Automatic compaction and recovery (next)** — Add `enabled`, one just-in-time threshold check before each normal Model Step, provider-aware measurement and summary input, Anthropic retained-thinking compatibility, narrow provider-specific overflow meaning and one same-step compact-and-retry. Demonstrate cancellation, branch/accounting continuity and bounded recovery without pre-request or post-final automatic work.
