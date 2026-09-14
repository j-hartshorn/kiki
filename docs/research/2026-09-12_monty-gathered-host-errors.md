# Monty gathered host-error scheduler stall

**Status:** Deferred. No Kiki workaround is planned unless the issue becomes materially disruptive.

## Summary

Monty can lose the useful Python exception when two or more host futures fail in one `asyncio.gather()` batch. The turn instead ends with an internal scheduler error:

```text
RuntimeError: Internal error in monty: asyncio scheduler stalled: no ready tasks and no pending external calls
```

This is a Monty scheduler defect rather than a subagent depth or concurrency protection. Kiki correctly rejects admissions and returns their errors, but Monty can tear down the gather while processing the first error and then find no runnable task when processing another error from the same batch.

The condition is deterministic with two gathered failing host calls, including ordinary `read()` calls, so it is not specific to recursive subagents. A host-future error inside a coroutine under `asyncio.gather()` can also bypass that coroutine's `try/except`. Sequential awaits behave normally.

In the recursive-subagent test, global concurrency pressure caused several gathered `start_subagent()` calls to be rejected together. The scheduler error obscured the underlying admission errors before task handles could be returned. The worker did not hang or terminate; only the current Python turn failed.

## Deferred direction

The preferred fix is in Monty's exception routing and gather scheduler, leaving Kiki's public async tool and subagent interfaces unchanged.

If Kiki needs an interim fix, expected host-tool failures could cross the private Monty seam as tagged return values and be raised by the generated Python wrappers. That would make exceptions originate inside the correct coroutine without changing the model-facing signatures or preventing successful calls from running concurrently. Do not require agents to serialise calls or avoid `asyncio.gather()` as the permanent solution.

## Upstream

- [pydantic/monty#839: deterministic host-future and gather reproduction](https://github.com/pydantic/monty/issues/839)
- [pydantic/monty#732: original scheduler-stall report](https://github.com/pydantic/monty/issues/732)
- [pydantic/monty#764: convert the scheduler assertion into a recoverable turn error](https://github.com/pydantic/monty/pull/764)
- [pydantic/monty#772: suspected origin of the current exception-routing behaviour](https://github.com/pydantic/monty/pull/772)
