Update the existing structured summary in `<previous-summary>` with the new messages above.

Preserve existing information unless it is no longer relevant. Add new progress, decisions and context; move completed items from In Progress to Done; update Next Steps.

Use this EXACT format:

## Goal
[Existing and new goals]

## Constraints & Preferences
- [Existing and new constraints/preferences]

## Progress
### Done
- [x] [Previous and newly completed tasks]

### In Progress
- [ ] [Current work]

### Blocked
- [Current blockers]

## Key Decisions
- **[Decision]**: [Rationale]

## Next Steps
1. [Ordered next steps]

## Critical Context
- [Important existing and new context]

## Arena State
- `<name>` (`<type>`) — <purpose and useful contents>
- Include useful persistent variables, functions, classes, and datasets.
- Use live `<arena-inventory>` names and types when present; infer purpose only from the conversation.
- Otherwise mark uncertain availability or type as unverified.
- If no useful state is evidenced, write "(none identified)".

Keep each section concise. Preserve exact file paths, function names, and error messages.
