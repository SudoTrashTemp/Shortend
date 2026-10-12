# ShortEnd

Verified media offloads for DIT work, built for Linux first. ShortEnd reads a camera card
once, writes it to several destinations at the same time, reads every copy back from the
drive, and only then marks it done. Each destination gets an ASC MHL manifest plus CSV, TXT and
PDF reports.

The engine is a Rust library (`shortend-core`). The `shortend` command line tool exposes all
of it; a GUI comes later (see [PLAN.md](PLAN.md)).

## Quick start

```sh
cargo build --release
./target/release/shortend copy -s /media/CARD_A001 \
    -d RAID=/mnt/raid/Day01 -d SHUTTLE=/media/shuttle/Day01 \
    --show "Big Show" --day 01 --camroll A001 --operator "Jane Doe"
```

- `shortend resume <job>` continues an interrupted job. Files already copied are checked
  again, and only files that are missing or don't match are copied.
- `shortend jobs` lists jobs and their state.
- `shortend verify <folder>` checks a delivered folder against its ASC MHL history or its MHL
  v1.1 files, without writing anything.

Exit codes: `0` means every file was verified on every destination. `1` means the job
finished with failures, which are named in the summary and in the reports. `2` means the job
was refused before it started, and nothing was written.

Checksums: xxh64 (the default), xxh3, xxh128, md5, sha1, sha256 and c4. Manifests: ASC MHL v2
(the default), MHL v1.1, both (`--mhl v2,v1.1`) or none.

## Filesystem support

ShortEnd checks every source and destination before copying. It refuses to start if it can't
write a destination safely, for example because the drive is read-only, too small, can't
store a file's name or size, or two names would collide on it.

### Support matrix

Key:
- **Tested**: covered by the automated tests on every push (CI).
- **Expected**: should work, but CI doesn't test it.
- **Refused**: ShortEnd stops before copying and explains why.

| Filesystem | Linux source | Linux destination | macOS source | macOS destination | Windows source | Windows destination |
|---|---|---|---|---|---|---|
| ext4 | Tested | Tested | — | — | — | — |
| XFS, Btrfs | Expected | Expected | — | — | — | — |
| exFAT | Tested | Tested | Expected | Expected | Expected | Expected |
| FAT32 | Tested | Tested ¹ | Expected | Expected ¹ | Expected | Expected ¹ |
| NTFS | Tested | Tested | Expected | Refused ² | Expected | Tested |
| HFS+ (not journaled) | Tested | Tested | Expected | Expected | — | — |
| HFS+ (journaled) | Tested | Refused ³ | Expected | Expected | — | — |
| HFSX (case-sensitive HFS+) | Tested | Tested | Expected | Expected | — | — |
| APFS | Tested ⁴ | Refused ⁵ | Tested | Tested | — | — |
| SMB / NFS shares | Expected | Expected | Expected | Expected | Expected | Expected |

"—" means the operating system can't mount that filesystem without third-party drivers.

The Linux results come from real filesystems: disk images created with each filesystem's own
format tool (`mkfs.*`, `mkapfs`) and attached as virtual disks. The device tests also cover:
- a drive unplugged mid-copy (simulated at the block-device level), for both the source and a
  destination;
- a drive that stops responding;
- a destination filling up;
- a drive switching to read-only mid-job;
- an unmount while files are still being copied;
- permission problems;
- name collisions.

On macOS and Windows, CI runs the full test suite (no drive simulation) on the runner's own
disk: APFS on macOS, NTFS on Windows.

¹ FAT32 can't store files of 4 GiB or more. A job whose card holds such a file is refused.

² macOS mounts NTFS read-only, so it is refused as a destination as with any read-only drive.

³ Linux mounts journaled HFS+ read-only. ShortEnd refuses it as a destination, explains this,
and gives the fix: turn off journaling on a Mac (`diskutil disableJournal /Volumes/NAME`), or
use an exFAT drive.

⁴ Read through `apfs-fuse`, which is read-only. ShortEnd identifies the APFS volume under
FUSE and applies APFS rules to it.

⁵ Linux can't write APFS reliably. ShortEnd refuses an APFS destination before copying,
explains why, and suggests exFAT, non-journaled HFS+ or ext4 instead. If an APFS volume is
mounted writable through the experimental `linux-apfs-rw` driver, ShortEnd warns but proceeds.

### Known limitations

- **Unicode names on Mac drives.** HFS+ stores file names in decomposed Unicode, so `é`
  becomes `e` plus a combining accent. ShortEnd treats both forms as the same name when
  checking for collisions and comparing against manifests. Tools that compare names
  byte-for-byte, including the reference `ascmhl` tool, may list such a file under its
  decomposed name.
- **Timestamps.** Modification times are preserved to the destination filesystem's
  precision: whole seconds on HFS+, 2 seconds on FAT32, 10 ms on exFAT, 100 ns on NTFS. This
  matters only for resume, which uses these times when matching files.
- **Resume on macOS** recognises a destination drive by its device number, which can change
  after the drive is reconnected. Linux and Windows match the volume's UUID or serial number.
  Fixing macOS is a blocker for the GUI milestone (PLAN.md, M4).
- **MHL v1.1 can't list zero-byte files**, because its schema requires a size of at least 1.
  If v1.1 is written alongside ASC MHL v2, the empty files appear only in the v2 manifest.
  The job then ends with exit code 1, and the summary and the CSV, TXT and PDF reports name
  each omitted file. If v1.1 is the only manifest selected and the card holds empty files,
  the job is refused before copying.
- **SHA-256 isn't part of the ASC MHL v2 schema.** A job whose only checksum is SHA-256 is
  refused when a manifest is enabled. If SHA-256 is computed alongside another checksum, it
  appears in the CSV, TXT and PDF reports only.
- **PDF reports** use the embedded DejaVu fonts. Characters those fonts lack, such as Chinese,
  Japanese and Korean, are shown as `[U+XXXX]` codes. The exact names are always in the CSV,
  TXT and MHL files.
- **Hung drives.** A drive that stops responding is dropped after the stall timeout (60 s by
  default), and the other destinations finish normally. This applies during setup, the resume
  check and the copy. The operating system may still hold on to the stuck drive until it is
  unplugged.

## Development

Run these before committing:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Some tests compare ShortEnd's output with reference tools: `ascmhl` 1.2, `mhl-suite` 1.3.1,
`xmllint`, poppler and `qpdf`. When a tool is missing, its check is skipped, unless
`SHORTEND_REQUIRE_TOOLS=1` is set, in which case it fails.

The device tests in `crates/shortend-cli/tests/linux_devices.rs` need root, and run only when
`SHORTEND_PRIVILEGED_TESTS=1` is set. See `.github/workflows/ci.yml` for the packages they need.

Licensing: the PDF fonts are DejaVu (see `crates/shortend-core/assets/fonts/`). The vendored
XSD schemas are under `xsd/`, with their own licence.
