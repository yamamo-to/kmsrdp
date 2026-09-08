#!/usr/bin/env bash
# beforeShellExecution: for `git commit`, point core.hooksPath at .githooks
# so fmt/clippy pre-commit always runs (including agent commits).
set -euo pipefail

input="$(cat)"
command="$(printf '%s' "$input" | jq -r '.command // empty')"

# Non-commit commands: allow.
if ! printf '%s' "$command" | grep -Eq '(^|[[:space:]])git([[:space:]]+.*)?commit([[:space:]]|$)'; then
  printf '%s\n' '{"permission":"allow"}'
  exit 0
fi

# Bypass / amend-only tooling still goes through git hooks unless --no-verify.
if printf '%s' "$command" | grep -Eq -- '--no-verify|-n([[:space:]]|$)'; then
  printf '%s\n' '{"permission":"deny","user_message":"git commit --no-verify is blocked; fix fmt/clippy instead.","agent_message":"Do not use --no-verify. Run cargo fmt / clippy and commit normally so .githooks/pre-commit can run."}'
  exit 0
fi

root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [[ -z "$root" || ! -x "$root/.githooks/pre-commit" ]]; then
  printf '%s\n' '{"permission":"allow"}'
  exit 0
fi

current="$(git -C "$root" config --get core.hooksPath || true)"
if [[ "$current" != ".githooks" && "$current" != "$root/.githooks" ]]; then
  git -C "$root" config core.hooksPath .githooks
fi

printf '%s\n' '{"permission":"allow","agent_message":"core.hooksPath=.githooks; pre-commit will run cargo fmt --check and clippy."}'
exit 0
