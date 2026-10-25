# ShortEnd — Work Order for Claude Code

> Written from a review of the repo (PLAN.md M1–M4 implemented, M5–M6 pending) against ShotPut Pro's
> documented feature set. All PLAN.md §1 guiding decisions stand unless explicitly revisited below.
> Work in priority order; within a priority, finish an item before starting another.

## Where the project stands (verified from PLAN.md §9 + README, 2026-10-25)

Done, with CI proof: fan-out copy with read-back verification and atomic commit; 7 checksums in one
read pass; ASC MHL v2 (default) + v1.1 with reference-tool validation (`ascmhl`, `mhl-suite`,
`xmllint`); CSV/TXT/PDF reports; TOML presets with ShotPut-compatible naming tokens; crash-safe
journal + resume (25× SIGKILL tested); stall watchdog; CLI (`copy`, `resume`, `jobs`, `verify`,
`preset`); egui GUI with automated widget tests on X11/Wayland; filesystem matrix (dm unplug,
disk-full, read-only remount); macOS volume-UUID resume. macOS is supported in CI — what remains
there is packaging (M6), not porting.

Pending: **M5** (format awareness + thumbnails) and **M6** (benchmarks + packaging + release).

## Priority 0 — Repo hygiene (one PR) — IN PROGRESS
1. Create `AGENTS.md` + one-line `CLAUDE.md` pointing at it. (done)
2. Audit M5/M6 checkboxes in PLAN.md §9 against reality — no aspirational ticks. (verified: all M5/M6
   boxes unchecked; no format/thumbnail code exists yet — only test fixtures match those names.)

## Priority 1 — M5: format awareness + thumbnails (PLAN.md §7, done-criteria in §9/M5)
Detection by extension + magic + card-folder signatures (ARRIRAW, Codex HDE, R3D spanned, X-OCN/XAVC,
BRAW, ProRes/DNxHR, CinemaDNG, crm/cr3, audio, sidecars); clip grouping so reports list clips not
40,000 frames; ffprobe metadata; I-frame thumbnails generated from the **verified destination copy**;
PDF clip section with ranges for frame sequences; full graceful degradation when ffmpeg is absent or
hangs. Meet PLAN.md §9/M5 verbatim.

## Priority 2 — M6: benchmarks, packaging, release
Bench/tune `parallel_files`/`chunk_size`/pool depth on NVMe/USB-HDD/NAS (`docs/bench.md`); static
musl CLI tarballs; AppImage on an older glibc base with the M4 runtime deps declared
(libGL/libEGL, libxkbcommon[-x11], libwayland-client); macOS universal `.dmg` with conditional
signing/notarization; Windows `.zip` + `.msi`; release workflow from tags; physical USB/eSATA/NVMe
pull checklist before release.

## Priority 3 — ShotPut Pro parity (revisits PLAN.md's "no auto-mount" exclusion deliberately)
1. **Queue mode** (auto-detect inserted removable volumes — Linux udev, macOS Disk Arbitration — and
   optionally auto-begin the active preset's job). Card allow-list by volume UUID. Off by default;
   preset opt-in. Overturning the §1 "no auto-mount" exclusion → record it in §1 with a dated note.
2. **Auto-eject source on completion** — only after all destinations verify; refuse if any failed.
3. **Completion signals** — local sound + desktop notification by default; email/SMS/push as an
   optional plugin only (core stays offline).
4. **Duplicate-offload detection** — warn at preflight when the same card (UUID) or same content
   (fast sentinel scan) already went to the same destination root. Warn loudly; refuse only on config.
5. **File include/exclude patterns** in presets — excluded files are never silent: listed in the
   report, noted in an MHL comment, and a named CSV column.
6. **Aggregate reports** — `shortend report <job-ids…>` (+ GUI) rolling up jobs into one "wrap report"
   PDF: totals per show/day/destination, pass/fail across the set.

## Priority 4 — Post-parity candidates (only after 1–3 ship)
Watch folders; S3/B2 cloud destination plugin (manifest-first, per-destination verification
semantics); drive benchmarking; `shortend seal <dir>` to append an MHL generation to a folder.

## Standing rules
See AGENTS.md. Key: fail closed; source tree provably untouched (release gate); exit codes are a
public API; reference tools decide MHL correctness; no dependency or §1-exclusion change without a
dated PLAN.md §1 note; `fmt`/`clippy -D warnings`/`test` before every commit.
