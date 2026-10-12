#!/usr/bin/env bash
# Regenerates the reference manifests under tests/fixtures with the reference tools:
#   ascmhl 1.2 (github.com/ascmitc/mhl)  and  simple-mhl from mhl-suite 1.3.1.
# Usage: crates/shortend-core/tests/fixtures/make_fixtures.sh   (needs both on PATH)
set -euo pipefail
cd "$(dirname "$0")"
# Note: the reference tool matches ignore patterns against absolute paths, so no folder on
# the path to a fixture may be named "ascmhl" (it would ignore every file).
rm -rf history mhl11
make_card() {
  local d=$1
  mkdir -p "$d/CLIP/A001C001_RDC" "$d/SOUND" "$d/Ünïcödé dir"
  printf 'clip one\n'            > "$d/CLIP/A001C001_RDC/A001C001_001.R3D"
  printf 'clip one, part two\n'  > "$d/CLIP/A001C001_RDC/A001C001_002.R3D"
  printf '<sidecar/>\n'          > "$d/CLIP/A001C001.xml"
  printf 'wav data\n'            > "$d/SOUND/T001.wav"
  printf '名前 & <odd> "name"\n' > "$d/Ünïcödé dir/名前 & x.txt"
  printf 'top level\n'           > "$d/README.txt"
  printf 'finder junk\n'         > "$d/.DS_Store"
}
for algo in xxh64 xxh3 xxh128 md5 sha1 c4; do
  make_card "history/$algo/CARD"
  ascmhl create -h "$algo" --author_name "Reference" "history/$algo/CARD" > /dev/null
done
# A history with two generations in two algorithms (xxh64, then md5 verified against it).
make_card "history/two_gen/CARD"
ascmhl create -h xxh64 "history/two_gen/CARD" > /dev/null
sleep 1
ascmhl create -h md5 "history/two_gen/CARD" > /dev/null

# MHL v1.1 sealed by simple-mhl (xxhash64be), plus a hand-written legacy file using the
# deprecated little-endian <xxhash64> and an <md5>.
make_card "mhl11/sealed/CARD"
rm "mhl11/sealed/CARD/.DS_Store"
simple-mhl seal -a xxh64 "mhl11/sealed/CARD" > /dev/null
make_card "mhl11/legacy/CARD"
rm "mhl11/legacy/CARD/.DS_Store"
python3 - "mhl11/legacy/CARD" <<'PY'
import hashlib, sys, xxhash, os
root = sys.argv[1]
rows = []
for f in ["README.txt", "SOUND/T001.wav"]:
    data = open(os.path.join(root, f), "rb").read()
    le = xxhash.xxh64(data).digest()[::-1].hex()
    rows.append(f"""  <hash>
    <file>{f}</file>
    <size>{len(data)}</size>
    <lastmodificationdate>2020-01-01T00:00:00Z</lastmodificationdate>
    <xxhash64>{le}</xxhash64>
    <md5>{hashlib.md5(data).hexdigest()}</md5>
    <hashdate>2020-01-01T00:00:00Z</hashdate>
  </hash>""")
open(os.path.join(root, "CARD_2020-01-01_000000.mhl"), "w").write(
    '<?xml version="1.0" encoding="UTF-8"?>\n<hashlist version="1.1">\n  <creatorinfo>\n'
    '    <username>ref</username>\n    <hostname>ref</hostname>\n    <tool>handwritten</tool>\n'
    '    <startdate>2020-01-01T00:00:00Z</startdate>\n    <finishdate>2020-01-01T00:00:00Z</finishdate>\n'
    '  </creatorinfo>\n' + "\n".join(rows) + "\n</hashlist>\n")
PY
echo "fixtures regenerated"
