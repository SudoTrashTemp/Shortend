# M5 — Format awareness + thumbnails: slicing & decisions

Breaks PLAN.md §7 into PR-sized slices and records the decisions that shape them. Done-criteria are
PLAN.md §9/M5, met verbatim. Each slice is one PR, pushed once, watched once (CI-minutes rule).

## Layering (frozen)

- Format awareness is **reporting, not copying**. The copy/verify engine and `FileEntry` stay
  untouched. New module `shortend-core/src/media/`:
  - `media/detect.rs` — pure classification (extension + magic + card-folder signature) → `MediaKind`.
  - `media/clip.rs` — pure clip grouping + frame-range computation.
  - `media/ffmpeg.rs` — the **only** place that spawns `ffprobe`/`ffmpeg`, behind a typed command
    boundary (like `fs/platform/`), per-call timeout that kills the child (ties to the job
    `stall_timeout_secs`, 60 s, so a hung child degrades on the drive clock). No `unsafe`; no
    `unwrap`/`expect` on child output.
- **Grouping runs on the SOURCE scan, metadata/thumbnails on the destination.** `scan.rs` already
  produces the full relpath listing; grouping is pure and reads no bytes, so running it on the source
  is free and safe (card untouched). Then the report stage **joins per-file copy outcomes by relpath**
  (zero extra I/O — the engine already holds them). ffprobe/thumbnails run on the **verified
  destination copy** as planned. Consequence (the integrity win): a gap that was **on the card**
  (aborted take) renders as a plain range; a gap caused by a **failed copy** renders highlighted and
  is named in the job summary (consistent with the existing anomaly-naming rule). The clip report is
  thus a true integrity artifact, not a fancy directory listing.
- New report data hangs off the report, never the copy model. CSV/TXT/MHL stay **per-file**; only the
  **PDF** rolls up to clips (Q4). CSV may gain an *additive* `clip_group` column; TXT may gain a
  per-job clip-summary block; **MHL is never touched** (a clip abstraction would be schema-wrong).

## Clip grouping rules (Q1, frozen — verify against real material before ticking)

Clip = files whose **normalized stem** matches a vendor pattern; otherwise one clip per file.
Stem comparison uses the **same name normalization `names.rs` uses for case-collisions**
(case-insensitive — that is where grouping bugs live). Originals preserved for display.

| Family | Grouping |
|---|---|
| R3D | spanned `..._001.R3D, _002.R3D` in an `.RDC` → one clip by stem |
| ARRIRAW `.ari`, Codex `.arx` | **numeric-suffix frame sequence → one clip; one per file otherwise** |
| CinemaDNG / DNG sequences | `CLIP_0000000.dng …` → one clip by numeric-suffix stem |
| ProRes/DNxHR/XAVC single container (`.mov`/`.mxf`) | one clip |
| BRAW | one clip (single container) |
| Sony XAVC-S / AVCHD spanned MP4/MTS (FAT32 4 GB splits) | strip sequential part suffix → group |
| Canon XF spanned MXF parts | strip part suffix → group |
| Canon CRM (Cinema RAW Light splits) | group `.crm` parts sharing the clip ID |
| Phantom `.cine`, Kinefinity `.krw` sequences | group by numeric-suffix stem |
| poly-WAV / audio | one clip per file |
| Sidecars (ARRI XML, `.M00/.M01`, XDCAM index, `.xmp/.ale/.cdl`/LUT) | **attach to a clip's stem; never form a clip; never counted in frame totals** |
| anything unrecognized | **one clip per file — never a refusal** |

## Slices

**① Detection + clip grouping (pure; source scan).**
`media/detect.rs` (`MediaKind`) + `media/clip.rs` (`Clip { stem, kind, members: Vec<RelPath>,
ranges, sidecars }`, grouping over the scan's relpaths, numeric-suffix ranges with **every gap shown
inline**, e.g. `STEM [000000–000512, 000521–001203] · 717 frames`). Reuse `names.rs` normalization
(factor out a shared `fold`). Tests: fixture headers per family; spanned/sequence/sidecar grouping;
case-insensitive stems; gaps → multiple ranges; unknown → one clip per file.

**② ffprobe metadata.** `media/ffmpeg.rs::probe` (codec/res/fps/timecode/duration), typed boundary +
timeout + child-kill. Attach to clips in the report stage.

**③ Thumbnails from the verified destination copy.** `media/ffmpeg.rs::thumbnail` (I-frame → `image`
→ 320 px long edge, JPEG q≈80). **Best-effort per codec** (see matrix). Probe/thumb run against the
**first destination, in `-d` spec order, that verified OK** (Q5); fall to the next verified only if
the first is unavailable at report time; degrade if none. One probe per clip.

**④ PDF clip section + report integration.** PDF clip section: kind, metadata, thumbnail or badge
placeholder, frame ranges; copy-failed sequence members highlighted + named in the summary (the Q2
integrity join). Additive CSV `clip_group` column; TXT clip-summary block. MHL unchanged.

**⑤ Degradation paths + done-criteria.** Tests for ffmpeg absent (`SHORTEND_NO_FFMPEG=1` / PATH
strip) and hanging (fake sleeping binary): job completes, PDF placeholders + one warning, timeout
respected, no crash. Tick PLAN.md §9/M5 per CI; append M5 notes to PLAN.md §7.

## Codec/thumbnail support matrix (Q3, frozen)

Thumbnails are **best-effort per codec** — fail-open, reporting-only.
- **Decodable** (stock ffmpeg + `image`): ProRes, DNxHR, H.264/HEVC in MXF/MP4, DPX, `.cine`.
- **Not decodable** (no thumbnail — media-type **badge placeholder**, noted in the PDF footer):
  ARRIRAW, R3D, BRAW, X-OCN, CRM, DNG. Vendor SDKs are out of scope.

## Test & CI contracts (Q6, frozen)

- Install `ffmpeg`/`ffprobe` on CI runners via the runner package manager (apt/brew/choco); set
  `SHORTEND_REQUIRE_TOOLS=1` in CI so their absence fails the job (same pattern as M1–M4 reference
  tools). Slice ⑤ additionally forces absent + hanging.
- **Never byte-golden ffmpeg output.** Thumbnail tests are **structural**: decode with `image`,
  assert dimensions + format + non-blank. ffprobe JSON fields and exit codes are stable contracts and
  may be asserted; pixels are not.
