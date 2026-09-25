# Monty REPL type-check limitations

By default, Kiki uses Monty 0.0.22's built-in `ty` check before model-authored Python runs. Sessions opted out with `KIKI_TYPE_CHECK=off` skip the check. Two worker behaviours can reject valid code:

- **Type context can diverge from Arena state.** Monty appends successful snippets verbatim to its type stubs, so `n = 1` followed by `n += 1` is rejected (`Literal[2]` cannot be assigned to `Literal[1]`). A snippet that raises is omitted from the type context even if earlier assignments survive at runtime. Imports made in one snippet are also unresolved in the next because the accumulated code is checked as a `.pyi` stub file. Repeating side-effecting calls to recover missing names can repeat external effects. See [Monty #872](https://github.com/pydantic/monty/issues/872) and the [rebinding reproduction](https://github.com/pydantic/monty/issues/872#issuecomment-5835667234).
- **Warnings also prevent execution.** After `n: int = 1`, `n += 1  # ty: ignore[unresolved-reference]` produces only `warning[unused-ignore-comment]`, yet Monty refuses the chunk. This does not have a linked upstream issue; suppressions are not a reliable general workaround.

These cases were reproduced with Monty 0.0.22. Kiki returns the diagnostics without running rejected code; it does not reconcile the worker's type context with live Python state.
