Summarise the conversation above as a structured checkpoint for another LLM to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

## Arena State
- `<name>` (`<type>`) — <purpose and useful contents>
- Include useful persistent variables, functions, classes, and datasets.
- Use live `<arena-inventory>` names and types when present; infer purpose only from the conversation.
- Otherwise mark uncertain availability or type as unverified.
- If no useful state is evidenced, write "(none identified)".

Keep each section concise. Preserve exact file paths, function names, and error messages.
