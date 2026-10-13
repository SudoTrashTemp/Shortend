# ShortEnd GUI — QA checklist

M4 done criterion: a full offload, cancel and resume can be done from the GUI on Linux (X11
and Wayland), macOS and Windows. Most of this is automated. This file records what the tests
cover, and what a person still has to check on each platform before a release.

## Automated (every push, CI)

| What | Where | Platforms |
|---|---|---|
| Check → Start → Cancel → History → Resume → PASS through the widgets; files identical on both destinations | `crates/shortend-gui/tests/gui.rs` (egui_kittest, headless) | Linux, macOS, Windows |
| Preflight refusal shown inline, Start disabled, stale results cleared when the form changes | `gui.rs` | all three |
| Preset editor validates, saves, refuses invalid TOML, applies to the form | `gui.rs` | all three |
| No repaint while idle (`Harness::run` settles in ≤3 frames, no repaint pending) | `gui.rs` | all three |
| ≥30 fps through a 10,000-file job with one destination hung and dropped by the watchdog (10,000 failures listed) | `crates/shortend-gui/tests/responsive.rs` | all three |
| The real window (eframe + OpenGL/Mesa) opens and keeps running | CI step "GUI smoke test" | Linux X11 (Xvfb), Linux Wayland (weston, headless) |
| `shortend-core` has no GUI dependency | CI step "Engine has no GUI dependencies" (`cargo tree`) | Linux |

Measured on the development machine (debug build, 1,000 × 800 window): 95th-percentile frame
3.5 ms and slowest frame 9.9 ms during the 10,000-file stalled-drive job. The idle release build
used 0 CPU ticks in 10 s under Xvfb.

## Manual, per release

Tick each box on each platform, and note the OS version and the date.

| Step | Linux X11 | Linux Wayland | macOS | Windows |
|---|---|---|---|---|
| App starts; window can be resized; nothing is clipped at 760 × 520 | ☐ | ☐ | ☐ | ☐ |
| Browse… opens the native folder picker (source, destination, logo) | ☐ | ☐ | ☐ | ☐ |
| Dropping a card folder on the window sets the source | ☐ | ☐ | ☐ | ☐ |
| Check shows the plan: files, size, each destination's target folder and filesystem | ☐ | ☐ | ☐ | ☐ |
| Start: progress bars and speeds move; current files listed | ☐ | ☐ | ☐ | ☐ |
| Pull a destination drive mid-copy: it is marked FAILED, the other continues | ☐ | ☐ | ☐ | ☐ |
| Cancel stops within a second or two; state "Cancelled" | ☐ | ☐ | ☐ | ☐ |
| History lists the job as cancelled; Resume finishes it; PASS | ☐ | ☐ | ☐ | ☐ |
| Closing the window during a copy asks first; "Cancel job and quit" leaves it resumable | ☐ | ☐ | ☐ | ☐ |
| "Open … reports" opens the report folder in the file manager | ☐ | ☐ | ☐ | ☐ |
| Idle app with no job: ~0% CPU in the system monitor | ☐ | ☐ | ☐ | ☐ |
| HiDPI / scaled display: text sharp, layout intact | ☐ | ☐ | ☐ | ☐ |

Known gaps:
- The native file dialogs use the XDG desktop portal on Linux. On a desktop without a portal
  (some minimal window managers), Browse… does nothing; type or paste the path instead.
- egui's built-in fonts have no CJK glyphs. Such file names show as boxes in the window, but
  are exact in the reports and manifests.
