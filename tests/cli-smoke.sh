#!/bin/sh
# Check command-line behaviour that needs no X server.
set -eu
bin=${1:-target/release/niruthemeselector}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail() { printf 'cli-smoke: %s\n' "$*" >&2; exit 1; }

version=$("$bin" --version)
field=$(printf '%s\n' "$version" | awk '{print $2}')
[ "$field" = "$(awk -F'"' '/^version = / {print $2; exit}' Cargo.toml)" ] ||
  fail "the second field of --version must be the version, got $field"
"$bin" --help | grep -q 'Usage: niruthemeselector' || fail '--help has no usage line'
if "$bin" nonsense >/dev/null 2>&1; then fail 'an unknown command succeeded'; fi
for command in status list; do
  if HOME="$tmp" PATH=/nonexistent NIRUTHEMESELECTOR_NIRUCTL="$tmp/missing" "$bin" "$command" >/dev/null 2>"$tmp/err"; then
    fail "$command succeeded without niructl"
  fi
  grep -q 'niructl' "$tmp/err" || fail "$command did not explain the missing niructl"
done
printf 'cli-smoke: ok\n'
