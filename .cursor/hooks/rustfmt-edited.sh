#!/usr/bin/env bash
# afterFileEdit: rustfmt any edited .rs file so fmt drift never reaches commit.
set -euo pipefail

input="$(cat)"
path="$(printf '%s' "$input" | jq -r '
  .file_path // .path // .filePath // .uri // empty
')"

# Some payloads nest the path.
if [[ -z "$path" || "$path" == "null" ]]; then
  path="$(printf '%s' "$input" | jq -r '
    .. | objects | .file_path // .path // .filePath // empty
  ' 2>/dev/null | head -n1 || true)"
fi

if [[ -z "$path" || "$path" == "null" ]]; then
  exit 0
fi

# Strip file:// prefix if present.
path="${path#file://}"

if [[ "$path" != *.rs ]]; then
  exit 0
fi

if [[ ! -f "$path" ]]; then
  # Relative to repo root.
  root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
  if [[ -f "$root/$path" ]]; then
    path="$root/$path"
  else
    exit 0
  fi
fi

rustfmt --edition 2021 "$path" 2>/dev/null || cargo fmt -- "$path" 2>/dev/null || true
exit 0
