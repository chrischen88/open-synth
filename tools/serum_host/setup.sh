#!/usr/bin/env bash
# Create tools/serum_host/.venv and install pinned deps. Idempotent.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VENV="$HERE/.venv"
PY="${PYTHON:-python3}"
if [ ! -x "$VENV/bin/python" ]; then
  "$PY" -m venv "$VENV" >&2
fi
"$VENV/bin/python" -m pip install --quiet --disable-pip-version-check --upgrade pip >&2
"$VENV/bin/python" -m pip install --quiet --disable-pip-version-check -r "$HERE/requirements.txt" >&2
echo "$VENV/bin/python"
