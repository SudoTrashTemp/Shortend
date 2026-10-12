# Vendored manifest schemas

Used only by tests and CI to validate every manifest ShortEnd writes (`xmllint --nonet --schema`).

| File | Source | License |
|---|---|---|
| `ASCMHL.xsd` | github.com/ascmitc/mhl `xsd/ASCMHL.xsd` (commit 0fb61f1, 2025-07-04), unmodified | MIT, see `LICENSE-ASC-MHL.md` |
| `ASCMHLDirectory.xsd` | same repository; the `<import>` points at the local `ASCMHL.xsd` instead of raw.githubusercontent.com so validation works offline | MIT |
| `MediaHashList_v1_1.xsd` | MHL v1.1 schema (mediahashlist.org) as bundled in `mhl-suite` 1.3.1 | as distributed there (MIT package) |
| `xmldsig-core-schema.xsd` | W3C XML Signature schema, imported by the v1.1 schema | W3C |
