# Do not add a built-in plan mode

Kiki will not implement a dedicated plan mode now and does not expect to add one later. Planning workflows will likely use Agent Skills instead, consistent with Kiki's [small-core direction](../VISION.md#a-small-core); this is a preferred direction, not a commitment to a particular skill.

The [reference-harness research](../research/2026-09-25_reference-harness-plan-mode.md) shows that dedicated modes add UI, session state, plan artefacts, and tool policy, while their read-only rules often leave other effect paths open. Skills are instructions, not an enforcement boundary: using one must not imply read-only execution or approval gating.
