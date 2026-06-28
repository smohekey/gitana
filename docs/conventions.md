# Conventions

Coding conventions for this project. Read this before writing or moving code.

## One type per file

Each Rust type (`struct`, `enum`, `trait`) lives in its **own file/module**, named after the
type, **unless it is ancillary to another type**.

A type is **ancillary** when it has **no implementations of its own** (only `derive`s) **and** is
referenced by **exactly one** other type — `struct` or `enum`, no exceptions. It lives in that one
type's file. When it gains its own `impl`s, or a second user, **re-evaluate**: it normally
graduates to its own file.

Decision rule:

| The type… | …goes in |
|---|---|
| has its own `impl`s (methods / hand-written trait impls) | **its own file**, always |
| no `impl`s, referenced by **exactly one** type | the **using type's** file (ancillary) |
| no `impl`s, referenced by **more than one** type | **its own file** |

Consequence: a purely behaviourless data model may legitimately live in **one file** (the root
type's), with types splitting out into their own files as `impl`s are added. That is expected, not
a smell.

### Examples

- An `enum Mode` with only `derive`s, used by both `Input` and `Output` → **its own file**
  (`mode.rs`), because more than one type uses it.
- An `enum Access` with only `derive`s, used only by `Mount` → lives in **`Mount`'s file**
  (ancillary).
- A `struct Node` with only `derive`s, used only by `Graph` → lives in **`Graph`'s file** — until
  `Node` gains `impl`s, when it graduates to `node.rs`.

### Notes

- A submodule's `mod.rs` holds the module's `mod`/`pub use` wiring (and short shared docs), not
  type definitions.
- **Small value types shared across modules may live directly in the crate root (`lib.rs`)**
  rather than a separate `common` module or their own files — until there are enough of them to
  warrant a module. (E.g. `Lifecycle`, shared by the definition and runtime layers.)
- "Used by" means *referenced in the type's definition* (a field, variant payload, or method
  signature) — not merely mentioned in functions.

## Imports and re-exports

- **Every `use` is grouped and fully path-prefixed** with `self::`, `super::`, or `crate::` —
  e.g. `use self::foo::{Bar, Baz};`. Never one `use` per item for the same path; never an
  unprefixed or glob import.
- **A parent module re-exports the public items of each child module it declares:**

  ```rust
  mod foo;
  pub use self::foo::{Bar, Baz};
  ```

  Use `pub` when the item is part of the crate's public API, `pub(crate)` when it is only used
  elsewhere within the crate.
- **Everything else refers to those items through the parent's namespace** (`super::Bar`,
  `crate::definition::Bar`) — never through the child module's own name. So **a module's name is written
  exactly once**: in its parent's `mod` / `pub use`. `foo::` is never qualified anywhere else.
- A module that *imports* a crate-local item brings it in with a **plain `use`** (via the nearest
  re-export); it does **not** itself re-export it. Only the declaring parent re-exports.

## Async

This project is **async-first**. Any operation that performs I/O or may block — invocation, backend
instantiation, handle await/stream/cancel, anything touching a transport — is exposed as an
**`async fn`** (or returns a `Future` / `Stream`). Synchronous wrappers are not provided by
default; a caller that needs one builds it.

- **Never block the runtime.** No blocking I/O or long CPU work on an async task; offload it
  (`spawn_blocking` or equivalent) and `.await` the result.
- **Cancellation is first-class.** Futures must be cancellation-safe — dropping one before
  completion must leave no half-applied state. This underpins handle cancellation and timeouts.
- **Async traits use native `async fn`** (edition 2024), not the `async_trait` macro.
