# `gitana-config` — Known Gaps and Follow-ups

Scoped, independent follow-ups against `gitana-config`. None blocks current work; each is recorded here
so it can be picked up without re-deriving why it matters, and so the next person who trips over one
finds the analysis instead of rediscovering it.

These were each found from *outside* the crate — while doing something else — which is why they had only
been noted in code comments at the point of use until now.

## Status

| Item | Severity | Found by | Status |
|---|---|---|---|
| `Debug` renders raw config values (credential exposure) | latent, sharp edge | unification slice 6 codex review | open |
| `[include]` / `includeIf` not expanded | correctness gap vs git | slice 4 (`list` tranche), round 8 | open |
| Leading UTF-8 BOM rejected | correctness gap vs git | slice 4 (`list` tranche), round 8 | open |

## `Debug` renders raw config values

`GitConfig`, `GitConfigSource`, and its element types all `#[derive(Debug)]`, printing every value
verbatim — **and again** in each element's `raw` source text. A merged stack routinely carries secrets:
an `http.extraHeader` holding `Authorization: Bearer <token>`, or a remote URL with an embedded token.
So `{:?}` on a config is a credential disclosure into any log line, trace, or error chain.

Verified by probe (unification slice 6):

```
effective: Some(GitConfig { sources: [ … value: Some("Authorization: Bearer SUPER_SECRET_TOKEN"),
  raw: "\textraHeader = Authorization: Bearer SUPER_SECRET_TOKEN\n" … ] })
```

**Currently latent.** No type in the workspace reaches a `Debug` impl holding a config: every use other
than `WorktreeContext` passes one as a by-reference function parameter and never formats it.
`gitana-linked-worktree`'s `WorktreeContext` was the first type to *hold* one, and it hand-writes a
redacting `Debug` (with regression tests in that crate's `tests/context.rs`).

**Borrowing is not the boundary** — a distinction this doc originally got wrong, caught in review.
`&GitConfig` implements `Debug` precisely because `GitConfig` does, so `{:?}` on a reference, or a
`Debug`-deriving struct with a `&'a GitConfig` field, leaks exactly as an owned one would. What keeps the
by-reference call sites safe today is that none of them formats the config — not the borrow. Any guidance
framed as "own = unsafe, borrow = safe" is worse than none: it licenses the leak it means to prevent.

**Why it is still worth fixing centrally.** The trap is reusable and silent: the *next* struct to hold a
`GitConfig` re-introduces the exposure by doing the most natural thing in Rust — deriving `Debug` — and
nothing warns. A per-embedder fix relies on every future author knowing this page exists. A redacting
`Debug` on `GitConfig`/`GitConfigSource` makes every embedder safe by default.

**The cost, and why this is a judgement call rather than an obvious fix.** Redaction degrades config
debugging exactly when it is most wanted (a layered-precedence bug is *about* which value won). Options,
roughly in increasing order of effort:

- Redact values, keep structure (section/subsection/key names, source order, which layer won). Keeps
  precedence debugging while dropping the secret-bearing part. Note keys alone can be sensitive
  (`credential.https://internal.example.com.username` names a host).
- Redact only known-sensitive keys (`http.extraHeader`, `credential.*`, anything URL-shaped). Better
  ergonomics, but it is a denylist — it fails open on the key nobody thought of, which is the wrong
  default for a disclosure boundary.
- Drop `Debug` entirely and offer an explicit `fn dump_unredacted(&self) -> String` for the debugging
  case. Fails *closed*, and makes disclosure a deliberate, greppable act.

The third is the most defensible; the first is the most convenient. Deferred pending a call on which.

## `[include]` / `includeIf` not expanded

git expands `[include] path = …` and conditional `includeIf` directives while parsing; gitana does not,
anywhere (`source.rs:57`, `lib.rs:7`, and noted at the CLI's config-read edge in `git_config.rs:98`). A
value set **only** via an include is therefore invisible to every gitana config read — not just one
command. `includeIf "gitdir:…"` is a common real-world pattern (per-directory identities), so this is a
plausible user-visible divergence, not a corner case.

Deferred deliberately at the end of the slice-4 `list` tranche: it is a pre-existing crate-wide gap, and
that tranche was already 8 codex rounds deep — the loop had descended from "real bug in the new code" to
"pre-existing feature gap," which is the signal to stop and get a merge decision rather than keep
widening scope.

## Leading UTF-8 BOM rejected

`GitConfigSource::parse` rejects a config file starting with a UTF-8 BOM; git accepts it. Editors on
Windows write one routinely, so a `.gitconfig` that git reads happily can be rejected outright by gitana.
Same provenance and same deferral reasoning as the includes gap above.
