# Spec: Model context compaction

## Context

Kiki persists immutable Session Entries and Arena Checkpoints, but the selected branch is still replayed in full for every model request. Long sessions will eventually exceed the selected model's context window. The current architecture also has two authorities for Model Context: `Agent` mutates an in-memory `ModelHistory`, while the session module independently reconstructs another history during restoration and interactive branch navigation (`src/agent.rs:537`, `src/session.rs:2754`, `src/runtime.rs:1092`). This conflicts with the intended ownership boundary in which the Session Model owns the Active Leaf and Model Context projection (`docs/VISION.md:134`).

This change adds Pi-compatible Model Context compaction. It replaces an older prefix of the selected branch with a durable summary while retaining the exact recent tail. It does not delete or rewrite Session Entries, Display History, or Arena Checkpoints. The Python Arena and external filesystem effects therefore survive unchanged.

Kiki also has continuity data which Pi does not: useful persistent Python bindings and structured filesystem effects. Compaction summaries include an evidence-based `Arena State` section and cumulative successful `read`, `write`, and `edit` paths so the next model can reuse both forms of state.

## Goals

- Make the Session Model the single owner of live and restored Model Context projection; remove independently mutable context ownership from `Agent`.
- Automatically compact using configured `enabled`, `reserve_tokens`, and `keep_recent_tokens`, while supporting `/compact [instructions]` below the threshold.
- Recover once from a recognised provider context overflow by compacting and retrying the same Model Step.
- Match Pi's summary, retained-tail, repeated-compaction, split-Run, output-budget, and cumulative read/modified-file behaviour, with one deterministic Kiki estimator and an evidence-based `Arena State` prompt extension.
- Persist every provider-started compaction outcome while the process remains alive through settlement and storage and Session invariants remain valid; only an applied outcome changes Model Context.
- Preserve cancellation, usage accounting, full Display History, branch navigation, and inspectability through collapsed transcript cards and durable session-tree rows.

## Non-goals

- Branch summarisation, branch merging, history deletion, or pruning stored Session Entries and Arena Checkpoints.
- A general message/command queue; Runtime retains at most one pending manual compaction, including across a pending correction.
- A generic session-storage port, alternate persistence backend, migration framework, or compatibility with pre-compaction format-version-1 sessions.
- Moving Ratatui rendering or terminal layout into the Session Model.
- A new per-Model-Step Runtime state machine; the existing Agent loop continues to orchestrate generation and Arena execution.
- Configurable summary models, thinking levels, retry counts, estimators, prompts, or split-Run policy.
- Authoritative Arena variable enumeration, Monty introspection changes, or claims that the evidence-based `Arena State` section is complete.
- Exact provider tokenisation, proactive summary chunking, truncated-output recovery, or a hard retained-tail token limit.
- New expansion controls or a compaction-specific viewer; transcript cards reuse global `Ctrl-O`, while the existing tree uses `Ctrl-X` copy.

## Key discoveries

- The intended Session Model already owns the Session tree, Active Leaf, committed records, Model Context projection, and Arena Checkpoint references (`docs/VISION.md:134`). Current code only partially follows this: `Agent` owns mutable `ModelHistory`, while restoration rebuilds it independently and Runtime installs it after model/Arena validation (`src/agent.rs:537`, `src/session.rs:2754`, `src/runtime.rs:701`, `src/runtime.rs:1092`).
- A `CompletedCode` Model Context entry keeps provider output, Python source, call identity, reasoning summary, and execution feedback together (`src/model.rs:138`, `src/model.rs:182`). Compaction can retain or summarise whole entries without separating a tool call from its result.
- OpenAI request construction is the current adapter from provider-neutral Model Context to provider replay (`src/openai.rs:161`). A synthetic compaction-summary entry can remain provider-neutral while each provider chooses its wire representation.
- Session Entries are immutable, and every non-execution entry must reuse its parent's Arena Checkpoint (`src/session/schema.sql:22`, `src/session/schema.sql:105`, `src/session.rs:2492`). Compaction can advance the Active Leaf without creating or mutating Arena state.
- Runtime already moves `Agent` into a foreground worker and returns it at settlement, while cloned `SessionWriter` handles share Arc-backed Session state with the controller (`src/runtime.rs:1355`, `src/runtime.rs:1721`, `src/session.rs:315`). Passing that existing shared handle to the worker avoids a per-step controller state machine or another ownership hand-off.
- User and project TOML settings already support strict field validation and field-wise precedence (`src/config/settings.rs:508`, `src/config/settings.rs:798`). `[compaction]` can extend this mechanism without session or environment overrides.
- Successful filesystem calls already carry normalised workspace-relative `ToolEffect` paths internally. New effect vectors can be copied into private execution metadata before that structure is discarded without adding them to settled display records; existing source/commentary can independently contain user-written lexical paths (`src/tools.rs:183`, `src/tools.rs:853`, `src/tools.rs:2295`).
- Monty checkpoints are opaque and Kiki's explicit result-value collection reads a designated dictionary rather than enumerating globals (`docs/research/2026-08-26_monty-feasibility.md:125`, `src/arena.rs:1041`). `Arena State` must therefore describe historically evidenced useful bindings and mark uncertainty.
- Pi's selected-path projection applies the latest compaction independently of richer session history, matching Kiki's existing separation between Model Context and Display History (`docs/research/2026-08-25_pi-architectural-lessons.md:56`, `docs/VISION.md:151`).
- Kiki's merged session tree decodes every durable entry, accounts for attempts on abandoned branches, and separately derives row rendering, search text, conversation-mode topology, and `Ctrl-X` copy (`src/session.rs:1965`, `src/session.rs:2175`, `src/tui/tree.rs:102`, `src/tui/tree.rs:1031`). A Compaction Entry must participate in each projection.
- Runtime and Transcript still model foreground work as a Run identified by `RunId` (`src/runtime.rs:335`, `src/transcript.rs:64`). Manual, pre-request, and post-final compaction require a foreground operation that can be active without inventing a Run.

## Proposed approach

### Canonical Session Model

Extend the existing session module into the canonical live and durable Model Context owner. It maintains the selected branch projection and supplies immutable snapshots to provider calls. A successful Session Entry commit updates that projection; failed storage leaves it unchanged. Restoration and live operation use the same projection rules.

Runtime remains responsible for provider validation and Arena restoration. After the target history and checkpoint validate, one guarded Session activation operation persists navigation selection when needed and publishes the matching immutable history only after commit. Startup activates the already selected projection through the same seam. Failed validation, Arena restoration, stale selection, or persistence leaves both the prior Active Leaf and live history unchanged.

Remove `model_context` from `Agent`. Agent continues to own the selected model and Arena and to orchestrate model generation and Python execution. The persisted root loop reads and commits context through the Session Model. Ephemeral subagent runs keep task-local history outside Agent rather than requiring a speculative storage abstraction.

Runtime moves Agent into the existing foreground worker and passes a cloned Session Model handle, retaining its own shared handle in the controller. Only Agent returns at settlement. Runtime records remain a Display History cache, not a Model Context authority. Navigation replaces the selected Session projection rather than mutating Agent history.

### Provider-neutral model operations

Extend the provider-neutral model contract with standalone summarisation. A summary request contains Kiki-owned instructions, serialised conversation text, the captured active model selection, and an output limit. A generated summary returns text and attempt accounting without provider replay.

The OpenAI adapter sends summaries through the Responses API with no tools, the captured active selection, normal streaming/cancellation/retry handling, and ordinary usage extraction. Summary output must be a complete, non-empty text response. Tool calls, incomplete responses, and output-limit truncation fail the compaction.

A provider-neutral compaction-summary Model Context entry owns its canonical text:

```text
The conversation history before this point was compacted into the following summary.

The Python Arena and its existing variables, functions, classes, and data persisted unchanged across compaction. Treat Arena details marked unverified as historical evidence and inspect them directly before relying on them.

<summary>
{{summary}}
</summary>
```

Estimation counts this same text. Provider adapters only encode it into their wire format, preventing wrapper drift. Future OpenAI, Google, and Anthropic adapters implement normal-step and summarisation operations without exposing wire formats to the Session Model.

### OpenAI context-overflow classification

The provider-neutral failure vocabulary exposes whether a normal Model Step failed from recognised context overflow. The OpenAI adapter classifies overflow as follows:

- An ASCII case-insensitive exact error code of `context_length_exceeded` or `model_context_window_exceeded` is recognised unless HTTP status is 401 or 429.
- Without one of those codes, only HTTP 400 or 413 is eligible. Its ASCII case-insensitive message must contain `exceeds the context window`, `maximum context length`, `context length exceeded`, or `context window exceeded`.
- Generic HTTP 400/413, authentication, rate-limit, `too many tokens`, and unrelated `maximum` messages remain ordinary provider failures.

Code matching takes precedence over message matching subject to the 401/429 exclusion. The OpenAI adapter classifies raw values before redaction/bounding and stores the semantic result beside bounded diagnostics. Overflow recovery consults this classification only for normal Model Steps; a similarly classified summary failure never recursively compacts.

After recovery starts, composable terminal evidence preserves one initial overflow observation alongside ordinary retry result/evidence. The initial observation owns its bounded structured failure, optional partial generation, and failed pre-compaction normal attempts; immediate overflow before any chunk stores no generation evidence. Retry code/final content or ordinary terminal generation/execution evidence remains the sole retry representation, and retry attempts remain exclusively on that code, Final, Failed, or Cancelled terminal's ordinary attempt field. Session totals include both attempt owners once, but context measurement never treats the initial attempts as post-compaction usage. The initial evidence survives successful final recovery and recovered code whose execution later fails or is cancelled. The final Run message reflects the last surrounding failure. Before retry, a reliable generation-epoch reset separates its live progress from delayed initial chunks while keeping the same durable Model Step. A process crash can lose in-memory evidence/attempts; existing generic interruption recovery uses durable rows only and does not invent them.

### Configuration and triggers

Add file-based user/project settings:

```toml
[compaction]
enabled = true
reserve_tokens = 16384
keep_recent_tokens = 20000
```

Fields use strict validation and field-wise project precedence. Numeric values must be positive and safely representable. There are no environment, session-preference, runtime-picker, or `Ctrl-S` overrides.

Automatic compaction uses the strict threshold:

```text
context tokens > model context window - reserve tokens
```

The subtraction saturates at zero when a configured reserve is at least the selected model's window. When enabled, check before a new user request reaches a provider, between a completed execution and the next Model Step, and after a final response before Run Settlement is reported to the interface. Pre-request measurement and preparation use only the currently selected durable Model Context; the held request is neither measured nor summarised, matching Pi's pre-prompt check. A retained manual compaction suppresses that post-final automatic check, then starts only after terminal publication and `RunSettled`. A non-cancellation threshold failure leaves Model Context unchanged and the existing workflow continues.

`/compact [instructions]` forces preparation below the threshold and remains available when automatic compaction is disabled. Each command receives an immediate Accepted, Busy, or Disabled acknowledgement before later lifecycle events; every acknowledgement, lifecycle transition, and durable card uses the same reliable FIFO event channel rather than lossy progress. The TUI permits only one unacknowledged Compact dispatch and retains a rejected command's exact editor draft. If a foreground Run is active, Runtime retains one pending manual compaction, cancels and settles the Run, then compacts the resulting Active Leaf with a fresh cancellation token. Esc received before that retained operation starts clears it, emits a reliable no-card `Settled(CancelledBeforeStart)` lifecycle, and still lets the already-cancelled Run/correction finish; no later compaction starts. If early-Escape correction is already pending, the retained compaction waits for that correction outcome and then targets whichever Active Leaf remains selected. Additional Submit or Compact commands are rejected busy; this is not a general queue. Background subagents continue because their child histories are isolated until explicitly retrieved.

A recognised provider context overflow compacts and retries the same pending Model Step once. A second overflow, a failed overflow compaction, or any different retry failure settles the Run normally as failed. `enabled = false` disables both threshold compaction and overflow recovery; manual compaction remains available.

### Deterministic context measurement

Search attempts newest-first on projected `CompletedCode` and `FinalMessage` entries whose durable Session Entries are descendants of the latest applied compaction. Retained ordinary ancestors appear after the synthetic summary in Model Context order but remain ineligible because their usage measured pre-compaction context; they participate only in full byte-estimation fallback. This includes terminal code only when it projects as completed code. Attempts on failed or cancelled terminals without projected code are ineligible because their output is absent from Model Context. Initial overflow attempts stored in recovery evidence are also always ineligible because they measured the pre-compaction request; only the retry's ordinary owning entry can supply a post-compaction baseline. `KnownUsage` and `KnownUsageWithUntrackedRetries` are both eligible; summary-call attempts are not. Untracked transport retries make cost completeness uncertain, but the final response's usage still measures the context of the response which enters Model Context. The first candidate with a non-zero context count is the baseline:

1. Use `total_tokens` when present and non-zero.
2. Otherwise require both `input_tokens` and `output_tokens` and use their checked sum when non-zero.
3. `cached_input_tokens` and `cache_write_input_tokens` are subsets of Kiki's OpenAI `input_tokens`; `reasoning_output_tokens` is a subset of `output_tokens`. Never add those detail fields again.
4. Missing, zero, or overflowing candidates are skipped. If none remains, estimate the complete projected context.

The baseline represents context through that assistant response. For a `CompletedCode` or terminal code response, estimate its later execution feedback but do not count its assistant replay again. Then estimate every later projected context segment.

Each projected entry contributes these estimation units:

- `UserRequest`: UTF-8 bytes of its text.
- Compaction summary: UTF-8 bytes of the complete fixed wrapper shown above plus its stored summary.
- `CompletedCode`, including terminal code: raw bytes of each opaque assistant replay item plus UTF-8 bytes of its execution feedback. Source, commentary, reasoning, and assistant text are not added separately because replay already represents the assistant response.
- `FinalMessage`: raw bytes of each opaque replay item; its decoded text and reasoning are not added again.
- Failed or cancelled terminal records without completed code: zero because they do not enter Model Context.

For a usage baseline inside `CompletedCode`, add only execution-feedback bytes from that same entry. A final-message baseline adds nothing from its own entry. Sum units with saturating arithmetic and divide the aggregate by four, rounding up. Full fallback, trailing additions, and retained-tail selection use this one recipe. It is deterministic and deliberately approximate rather than pretending to be a provider tokenizer.

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

Serialise the selected prefix with explicit user, assistant reasoning, assistant action, and execution-result labels. Phase 2's implementation plan pins the byte-level mapping of every current Model Context field, separators, wrappers, empty optional fields, split-prefix fallback, and focus suffix. Truncate each execution result to Pi's 2,000-Unicode-scalar summarisation limit without altering durable history. A repeated summary is supplied inside `<previous-summary>` tags. Conversation text is untrusted material to summarise, not an instruction source; fixed system and summary prompts retain authority.

### Arena and file continuity

Compaction reuses the prepared Active Leaf's exact Arena Checkpoint. It never creates, restores, probes, or mutates the Arena.

Record normalised workspace-relative paths for successful root-agent `read`, `write`, and `edit` results which the Arena driver observes and settles. Do not separately expose the new effect vectors through tool summaries, ordinary cards, tree execution rows/search/copy, or execution feedback. Existing Python source and commentary remain unchanged and can independently contain lexical paths. Ordinary Python errors commit their resulting Python state and observed file effects. Cancellation discards tentative Python state by restoring the prior checkpoint, but its evidence still retains already-settled external file effects.

This is useful continuity metadata, not a complete filesystem audit. A tool can finish its external mutation while cancellation wins before the driver observes its result; that path is unknowable and may be absent. Existing Session Entries also have no paths to backfill. Compaction never infers missing operations from current checkpoint authorisations.

For each preparation, derive cumulative sets from immutable root-agent execution metadata on selected ancestry through the end of the newly summarised range. Deduplicate and sort paths. A path which was modified at any included point appears only in `<modified-files>`, never also in `<read-files>`. Both `write` and `edit` count as modified. Append only non-empty Pi-style sections to the stored Markdown summary; do not persist a duplicate cumulative-list authority on the Compaction Entry. Retained-tail operations are included only when that tail is absorbed by a later compaction. Child-agent file activity remains in child history and reaches root context only through explicitly retrieved child results.

The `Arena State` section remains model-authored evidence. Kiki does not parse checkpoints or claim that listed bindings still exist or retain the inferred type.

### Preparation and durable outcomes

Preparation is a read-only Session Model operation. It captures the expected Active Leaf, context measurement, summary ranges, first retained entry, previous summary, cumulative files, prompt inputs, and reason. No SQLite transaction remains open while the model generates summaries.

Subject to the process remaining alive through settlement and storage remaining writable, every provider-started compaction settles as an immutable branch-local Session Entry:

- **Applied**: final summary, first retained entry, context measurement, reason, and summary attempts.
- **Failed**: diagnostic bounded to the first 4,096 UTF-8 bytes at a valid scalar boundary with no truncation marker, reason, and summary attempts.
- **Cancelled**: reason and summary attempts.

Cancellation before a provider attempt starts writes nothing. Failed and cancelled entries remain inspectable and count towards session usage, but Model Context ignores them.

A normal commit verifies that the expected Active Leaf is still selected, reuses its Arena Checkpoint and Run provenance, appends the outcome, and advances the Active Leaf atomically. Compaction is not a Model Step and does not create a synthetic Run or request. Applied, Failed, and Cancelled outcomes are valid branch points: selecting one in `/tree` restores through that entry and a later request becomes its child. Applied changes the projected prefix; Failed and Cancelled retain their parent's Model Context.

The current single-foreground-operation design makes a stale prepared leaf unreachable. Keep the expected-leaf guard, but do not add speculative off-branch settlement machinery. A mismatch is an invariant/persistence failure: append nothing, leave the current projection unchanged, and disable further state-changing work through existing policy. Storage failure or process loss before append is the physically unavoidable exception to durable outcome recording. Compaction deliberately creates no in-progress Run, Model Step, or transaction journal, so restart recovery does not invent a lost manual, pre-request, inter-step, post-final, or overflow summary attempt.

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
- once the append transaction starts, the chosen outcome wins. Later Esc cannot rewrite or roll back it.

Cancellation observed by the linearisation point has these surrounding effects:

| Compaction point | Surrounding result |
| --- | --- |
| Manual compaction | Leave context unchanged and return idle. |
| Threshold before request admission | Do not commit or start the submitted Run. |
| Threshold between Model Steps | Settle the containing Run as cancelled. |
| Threshold after committed final response | Keep the successful Run and report settlement. |
| Overflow recovery | Settle the pending Run as cancelled while retaining overflow evidence and accounting. |

If Esc arrives after an `Applied` transaction starts, the compaction remains applied. Manual and post-final work then returns/settles normally; pre-request admission does not start the submitted Run; an in-progress Run settles cancelled before another normal Model Step; overflow recovery does not issue its retry. A cancellation before a summary provider attempt writes no compaction outcome but has the same surrounding result. A manual compaction retained behind correction starts only after correction and `RunSettled`, always preparing against the resulting selected leaf, including when correction is skipped or fails without disabling the Runtime. Esc before `Started` cancels only that retained follow-up through `Settled(CancelledBeforeStart)` while correction/Run settlement continues. If Final or Failed wins the cancellation race and no correction event occurs, terminal publication and `RunSettled` still precede retained manual compaction from that terminal leaf. In either path, settlement which disables state-changing work clears the retained request. Non-cancellation manual failure returns idle. Non-cancellation threshold failure records its attempt and continues. Non-cancellation Failed/NoOp overflow recovery closes the Run with this exact message: `context overflow: {original}\ncompaction recovery failed: {recovery}`. Each substituted component is independently truncated to its first 2,000 UTF-8 bytes at a scalar boundary with no marker, so the complete message is at most 4,047 bytes. `{original}` is the bounded/redacted provider failure display; `{recovery}` is the Failed Compaction diagnostic or literal `compaction could not remove a valid prefix` for NoOp. A Failed Compaction Entry retains its complete separately bounded 4,096-byte diagnostic, while the Run message deliberately repeats only that 2,000-byte excerpt. Storage failure follows existing read-only/disabled policy.

Normal Model Steps, failed overflow attempts, and summary calls retain separate attempt accounting. Applied, failed, and cancelled compaction attempts contribute to session totals exactly once. Summary usage never replaces the latest normal context measurement shown in the footer.

### Presentation

While generating a summary, the status line reads `compacting context`; Esc remains the cancellation control. Compaction is foreground work for submission, tree-navigation, busy, shutdown, and settled-state checks even when no Run ID exists.

Applied transcript entries render collapsed by default:

```text
╭compaction · automatic threshold
╰Compacted 1.03m tokens · Ctrl-O to expand
```

Manual and overflow entries identify their reason. In the transcript, `Ctrl-O` reveals the exact stored Markdown summary, including `Arena State`, cumulative file sections, and summary-call accounting. Failed and cancelled entries use the same compact card shape with bounded diagnostics. Full pre-compaction Display History remains visible and selectable.

`/tree` shows Applied, Failed, and Cancelled Compaction Entries in both full and conversation modes with a distinct semantic colour. Exact rows are `Compaction · {reason} · applied · {measurement} tokens`, `Compaction · {reason} · failed`, and `Compaction · {reason} · cancelled`; reason labels are `manual`, `automatic threshold`, and `context overflow`, and measurement uses the existing footer token formatter. Search canonicalises only the complete row text, not focus, summary, or diagnostics. The entry participates in latest-descendant branch ordering. `Ctrl-X` copies only the outcome payload: stored summary for Applied, bounded diagnostic for Failed, and exact reason label for Cancelled, each passed through the tree's existing terminal-control normalisation before reaching the clipboard. Tree rows remain single-line and do not use transcript `Ctrl-O` expansion.

## Key decisions & tradeoffs

- Session Model ownership over Agent ownership or a standalone Compactor: one authority serves live operation, restoration, and future branch selection.
- A provider-neutral summarisation operation over OpenAI-specific orchestration: future adapters own their wire formats without a speculative provider registry.
- The active model and thinking selection over a cheaper fixed summary model: no extra configuration, credentials, or quality policy.
- Pi's reserve and retained-tail defaults over percentages: proven behaviour and predictable output headroom.
- One byte-based estimator over provider tokenisers: deterministic handling of opaque replay with no new dependency, at the cost of not reproducing Pi's UTF-16 arithmetic exactly.
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
- Approximate estimation can trigger early or late, especially with opaque replay and large tool results. Overflow recovery covers recognised late cases, not unknown provider failures.
- Provider overflow diagnostics can change. The deliberately narrow classifier may need a future provider-specific update rather than broad message guessing.
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
- Immutable npm package `@earendil-works/pi-coding-agent@0.85.1` (integrity `sha512-FGRN+OHbWaefBPGaTggAdLjrIHW+s2PzLyglz/5dfLzb9of7uuXMXYC0fJIeZTw+shS32o2cuQ9jF7YSDuL/oQ==`) and Pi session-format documentation supply the behavioural and prompt baseline inspected during planning. Kiki copies the four prompt resources into this repository and has no runtime Pi dependency.
- Existing OpenAI Responses API adapter, layered TOML settings, SQLite session storage, Runtime worker, transcript cards, and global expansion control. No new crate is required.

## Phase outline

Each phase is one reviewed PR. Later phases build on accepted behaviour and interfaces rather than carrying temporary alternatives.

1. **Canonical Session Model context** — Move root Model Context ownership out of Agent and into the Session Model while preserving all existing live, restored, navigated, subagent, correction, replay, and durability behaviour. Demonstrate that live continuation, branch selection, and restart use the same committed projection.
2. **Manual compaction and continuity** — Advance sessions to format version 2; add `reserve_tokens` and `keep_recent_tokens` settings, prompt resources, provider summarisation, deterministic preparation, best-effort file-operation persistence, durable outcomes/projection, transcript/tree presentation, and `/compact [instructions]`. Demonstrate manual/repeated compaction, direct tree selection and branching, restart, exact Arena continuity, cumulative modified-dominates-read files, correction-following queueing, and selected/off-branch accounting. The `enabled` field remains absent because no automatic behaviour exists yet.
3. **Automatic compaction and recovery** — Add the `enabled` setting, all three threshold boundaries, OpenAI overflow classification, and one compact-and-retry of the same pending Model Step. Demonstrate bounded recovery and the complete cancellation matrix.
