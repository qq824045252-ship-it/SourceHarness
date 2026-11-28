# SourceHarness Engineering Rules

1. Cross-platform first: every tool and test must run natively on Windows, Linux, and macOS. Never add OS-specific sandboxes (bwrap, seccomp, openat2, rlimit) unless explicitly requested.
2. Single active architecture: never keep dual profiles (`classic` vs `compact`) or speculative experiment flags (`history_mode`, `receipt_mode`, `stale_projection`, `edit_mode`). Replace, don't accumulate.
3. Append-only model prefix: keep `instructions` and `tools` static, and never mutate historical items in `input`.
4. Permissive input, strict core: never put `#[serde(deny_unknown_fields)]` on model-facing tool arguments.
5. Justify every guard: only keep runtime guards backed by real observed failures (loop guard, workspace path containment, optimistic revision conflict, atomic journal recovery).
6. Keep core small: target < 3,500 lines of Rust in `src/`.
