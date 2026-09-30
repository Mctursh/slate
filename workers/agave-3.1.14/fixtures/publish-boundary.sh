#!/bin/bash
# Compress each boundary fixture, record its sha256, and print the upload command.
# The checksum file is committed; the .zst files are release assets.
set -euo pipefail
cd "$(dirname "$0")/boundary"

shopt -s nullglob
fixtures=(*.slfix)
if [ ${#fixtures[@]} -eq 0 ]; then
  echo "no .slfix files here; run the extractor first"; exit 1
fi

for f in "${fixtures[@]}"; do
  [ -f "$f.zst" ] || { echo "compressing $f"; zstd -19 -q --force -o "$f.zst" "$f"; }
done

{
  echo "# sha256 of each published boundary fixture, verified before the file is trusted."
  echo "# Regenerate with: ./fixtures/publish-boundary.sh"
  echo "# Format: <sha256>  <filename>"
  shasum -a 256 ./*.slfix.zst | sed 's| \./| |'
} > checksums.txt

echo
echo "wrote checksums.txt:"
grep -v '^#' checksums.txt | sed 's/^/  /'
echo
echo "now upload, then commit checksums.txt:"
echo "  gh release create fixtures-v1 --title 'Boundary fixtures v1' --notes 'Tier-2 boundary fixtures' ./*.slfix.zst"
echo "  (or: gh release upload fixtures-v1 ./*.slfix.zst --clobber)"
