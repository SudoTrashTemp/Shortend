# ShortEnd: working agreement

Read this before changing anything. The design is in [PLAN.md](PLAN.md); the current build order is
[docs/work-order.md](docs/work-order.md). Milestone state and done-criteria live in PLAN.md §9.

ShortEnd is a Linux-first, verified media offloader for DIT work: one source copied to N
destinations, every copy proven by read-back against a single in-stream source hash, with ASC MHL
manifests and CSV/TXT/PDF reports. Core is a Rust library (`shortend-core`); `shortend-cli` and
`shortend-gui` are thin shells over it.

## Ground rules (binding)

1. **PLAN.md §1 decisions are binding.** The exclusions there (no async runtime, no plugin system,
   no database, no logging framework, offline/no-network core) hold unless a work-order item
   explicitly overturns one — and an overturn is recorded in §1's table with a dated note, the way
   the M1–M4 implementation notes already are.
2. **Fail closed.** A refused job (exit 2) beats a wrong copy. No new "warn and proceed" path
   without a preset knob *and* a report line. Anything unproven is treated as failure, not success.
3. **The source tree is never written.** A test asserts the card is byte-for-byte untouched after a
   job; that assertion is a **release gate**, not a nicety. Keep it passing.
4. **Exit codes are a public API — never renumber.** `0` success · `1` finished with failures ·
   `2` refused before writing · `3` cancelled · `4` internal error. (`EXIT_*` in
   `crates/shortend-cli/src/main.rs`.) README and reports depend on these.
5. **Reference tools decide MHL correctness**, not our reading of the spec: `ascmhl`, `mhl-suite`,
   `xmllint --schema` in CI are the arbiters. If our output and a reference tool disagree, we are
   wrong until proven otherwise.
6. **No new dependency without amending PLAN.md §1** with the same why-column rigor used there
   (what it's for, why it over alternatives, what it pulls in).
7. **Every milestone keeps the M-pattern:** plan in PLAN.md, implementation notes appended in the
   same milestone section, done-criteria ticked only when CI proves them. No aspirational ticks.

## Before every commit

```
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
All three must pass. One feature per PR; the PR that ships a feature updates its PLAN.md §9 row and
appends its implementation notes in the same PR. Conventional commit messages (`feat:`, `fix:`,
`docs:`, `test:`, …). No force-unwraps / `unwrap()` / `expect()` on data read from disk or a card;
fuzz malformed inputs.

## Toolchain note (sandbox)

Rust is installed under `~/.cargo`. If `cargo` isn't found, the shell didn't pick up the persistent
PATH — run `export PATH="$HOME/.cargo/bin:$PATH"` (already appended to `/etc/sandbox-persistent.sh`)
or use a login shell. Some tests need external tools (`ascmhl`, `mhl-suite`, `xmllint`, `ffmpeg`)
and privileges (dm-error, FUSE); CI is the source of truth for those.
