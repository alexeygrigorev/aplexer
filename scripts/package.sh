#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
NAME=${1:-aplexer-implementation}
DEST=${2:-"$ROOT/dist"}
mkdir -p "$DEST"
rm -f "$DEST/$NAME.tar.gz" "$DEST/$NAME.zip"
find "$ROOT" -type d \( -name target -o -name __pycache__ -o -name .pytest_cache -o -name .git \) -prune -exec rm -rf {} + 2>/dev/null || true
(
  cd "$ROOT"
  find . -type f -not -path './dist/*' -not -name MANIFEST.sha256 -print0 \
    | sort -z \
    | xargs -0 sha256sum > MANIFEST.sha256
)
parent=$(dirname "$ROOT")
base=$(basename "$ROOT")
(
  cd "$parent"
  tar --exclude="$base/dist" --exclude="$base/target" --exclude="$base/.git" \
      --sort=name --owner=0 --group=0 --numeric-owner \
      -czf "$DEST/$NAME.tar.gz" "$base"
)
if command -v zip >/dev/null 2>&1; then
  (
    cd "$ROOT"
    find . -type f -not -path './dist/*' -print | LC_ALL=C sort | zip -q "$DEST/$NAME.zip" -@
  )
fi
artifacts=("$DEST/$NAME.tar.gz")
[ -f "$DEST/$NAME.zip" ] && artifacts+=("$DEST/$NAME.zip")
sha256sum "${artifacts[@]}" > "$DEST/SHA256SUMS"
