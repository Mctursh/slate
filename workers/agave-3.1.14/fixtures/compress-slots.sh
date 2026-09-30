#!/bin/bash
# Tier-1 slot fixtures are committed compressed. Re-run after any re-capture.
set -euo pipefail
cd "$(dirname "$0")/slots"
shopt -s nullglob
for f in *.slfix; do
  echo "compressing $f"
  zstd -19 -q --force -o "$f.zst" "$f"
done
ls -l ./*.slfix.zst | awk '{printf "  %-30s %.1f MB\n",$9,$5/1048576}'
