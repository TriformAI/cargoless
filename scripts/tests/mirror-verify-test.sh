#!/usr/bin/env bash
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
verify="$repo_root/scripts/mirror-verify"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/cargoless-mirror-verify.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

git init --bare -q "$tmp/origin.git"
git init --bare -q "$tmp/github.git"
git init -q "$tmp/work"
git -C "$tmp/work" config user.name mirror-test
git -C "$tmp/work" config user.email mirror-test@invalid
git -C "$tmp/work" remote add origin "$tmp/origin.git"
git -C "$tmp/work" remote add github "$tmp/github.git"
printf 'first\n' > "$tmp/work/payload"
git -C "$tmp/work" add payload
git -C "$tmp/work" commit -qm first
git -C "$tmp/work" branch -M main
first=$(git -C "$tmp/work" rev-parse HEAD)
git -C "$tmp/work" push -q origin HEAD:main
git -C "$tmp/work" push -q github HEAD:main
(cd "$tmp/work" && "$verify" "$first" main >/dev/null)

printf 'second\n' >> "$tmp/work/payload"
git -C "$tmp/work" commit -qam second
second=$(git -C "$tmp/work" rev-parse HEAD)
git -C "$tmp/work" push -q origin HEAD:main
if (cd "$tmp/work" && "$verify" "$second" main >/dev/null 2>&1); then
  printf 'mirror-verify-test: expected a stale GitHub mirror to fail\n' >&2
  exit 1
fi
git -C "$tmp/work" push -q github HEAD:main
(cd "$tmp/work" && "$verify" "$second" main >/dev/null)

printf 'mirror-verify-test: GREEN\n'
