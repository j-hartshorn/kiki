# Research: Monty versus Python for Kiki

## Scope

Code-writing differences relevant to Kiki's [pinned Monty 0.0.22 worker](../../Cargo.toml#L17-L18), not an exhaustive CPython compatibility catalogue. Sources: the local `.research/monty/source` checkout at `6b64bd8`, its documentation and runtime code, Kiki's arena, and the bundled prompt.

## Summary

- Monty implements a deliberate subset of CPython 3.14; unsupported syntax is rejected before execution, and ordinary builtins and modules can be absent. [Subset](https://github.com/pydantic/monty/blob/6b64bd8/docs/python-subset.md#language-features) · [Builtins](https://github.com/pydantic/monty/blob/6b64bd8/limitations/builtins.md) · [Modules](https://github.com/pydantic/monty/blob/6b64bd8/limitations/modules.md)
- Generator expressions produce lists, as do `enumerate`, `zip`, `map`, `filter` and `reversed`; `next(x for x in xs)` therefore raises `TypeError`, and consuming an infinite iterator with the eager builtins is unbounded. [Parser](https://github.com/pydantic/monty/blob/6b64bd8/crates/monty/src/parse.rs#L1433-L1448) · [Builtins](https://github.com/pydantic/monty/blob/6b64bd8/limitations/builtins.md#behavioural-divergences)
- F-strings work; `format()`, `str.format()` and string `%` formatting do not. Many built-in methods can be called directly but cannot be retrieved as values. [Formatting](https://github.com/pydantic/monty/blob/6b64bd8/limitations/format.md) · [List method dispatch](https://github.com/pydantic/monty/blob/6b64bd8/crates/monty/src/types/list.rs#L600-L611) · [Attribute lookup fallback](https://github.com/pydantic/monty/blob/6b64bd8/crates/monty/src/types/py_trait.rs#L664-L666)
- Kiki supplies explicit host tools for I/O. It does not service Monty OS suspensions or unknown names; its tool reference already says to await host calls and restricts `asyncio.gather` to independent work. [Arena](../../src/arena.rs#L1673-L1748) · [Tool reference](../../src/tools.rs#L642-L668)

## Findings

Monty's parser rejects `yield`, class inheritance, method decorators, `match`, `del`, `async with`, `async for`, async comprehensions and wildcard imports. Simple functions, containers, comprehensions and `await` do work; `asyncio` exposes `run` and `gather` but not a sandbox event loop. [Subset](https://github.com/pydantic/monty/blob/6b64bd8/docs/python-subset.md#language-features) · [Asyncio](https://github.com/pydantic/monty/blob/6b64bd8/limitations/asyncio.md)

The [module registry](https://github.com/pydantic/monty/blob/6b64bd8/crates/monty/src/modules/mod.rs#L39-L113) and [documented inventory](https://github.com/pydantic/monty/blob/6b64bd8/limitations/modules.md) list 16 importable stdlib modules in production; `gc` is test-hook-only, and there is no `sys.path` or third-party package loading. The APIs are partial: [`asyncio`](https://github.com/pydantic/monty/blob/6b64bd8/limitations/asyncio.md) has `run` and `gather`, [`functools`](https://github.com/pydantic/monty/blob/6b64bd8/limitations/functools.md) has `reduce` and `partial`, [`collections`](https://github.com/pydantic/monty/blob/6b64bd8/limitations/collections.md) has four container types, and [`json`](https://github.com/pydantic/monty/blob/6b64bd8/limitations/json.md) includes `loads`, `dumps` and `JSONDecodeError`. Synchronous callbacks, including `reduce`, cannot suspend to call host tools. [Functools](https://github.com/pydantic/monty/blob/6b64bd8/limitations/functools.md#behavioural-divergences)

Monty's file and OS operations can be host-mediated in other deployments, but Kiki returns `NotHandled` for OS calls; its configured tools are the available path to the host. [Monty filesystem](https://github.com/pydantic/monty/blob/6b64bd8/docs/filesystem.md) · [Kiki arena](../../src/arena.rs#L1742-L1749)

Kiki keeps Monty state between executions and commits ordinary Python errors and prior mutations. The system prompt already says this, and the generated tool reference already covers awaiting calls, batching and output size. The primer is the only place it describes language incompatibilities. [Arena](../../src/arena.rs#L1005-L1068) · [Error checkpoint](../../src/arena.rs#L1492-L1544) · [Prompt](../../prompts/system.md) · [Tool reference](../../src/tools.rs#L642-L668)

## Key references

- [Monty's Python subset](https://github.com/pydantic/monty/blob/6b64bd8/docs/python-subset.md) and [limitations catalogue](https://github.com/pydantic/monty/tree/6b64bd8/limitations) — upstream reference for language and library gaps.
- [`src/arena.rs:1673`](../../src/arena.rs#L1673) — Kiki's actual host-call and OS boundaries.
- [`prompts/system.md`](../../prompts/system.md) and [`src/tools.rs:642`](../../src/tools.rs#L642) — model-visible instructions and tool reference.
