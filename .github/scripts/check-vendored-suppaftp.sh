#!/usr/bin/env bash
# vendor/suppaftp must be the suppaftp crate published on crates.io plus
# exactly the changes documented in vendor/suppaftp/PATCHES.md:
# - src/lib.rs differs by vendor/suppaftp.patch and nothing else;
# - LICENSE-MIT and LICENSE-APACHE (upstream's, by hash) and PATCHES.md are
#   added;
# - every other file is byte-identical, and none is missing or extra.
# Cargo.lock keeps no checksum for a [patch.crates-io] path dependency, so
# this is what ties the vendored code to the published crate.
set -euo pipefail

version=10.0.2
# the crates.io index checksum of suppaftp 10.0.2
crate_sha256=821001051ea3d12a60fb790b8c7cb9a6f5f8698dcfdca4cd533a025fefb0b5b8
# the licence files of veeso/suppaftp at 194bdd1979b16c4848d1fad6897dfa524b688d88
mit_sha256=4e883a0c89656afe3aaa559b3d4096a8aaa7534f00937f1c0054e231b2928078
apache_sha256=c6596eb7be8581c18be736c846fb9173b69eccf6ef94c5135893ec56bd92ba08

root="$(git rev-parse --show-toplevel)"
vendored="$root/vendor/suppaftp"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fail() {
  echo "::error::vendor/suppaftp: $*"
  exit 1
}

curl -sSfL --retry 3 -o "$work/suppaftp.crate" \
  "https://static.crates.io/crates/suppaftp/suppaftp-$version.crate"
echo "$crate_sha256  $work/suppaftp.crate" | sha256sum -c - \
  || fail "the downloaded crate does not match its crates.io checksum"
tar -xzf "$work/suppaftp.crate" -C "$work"
upstream="$work/suppaftp-$version"

list() {
  (cd "$1" && find . -type f | LC_ALL=C sort)
}

removed="$(LC_ALL=C comm -23 <(list "$upstream") <(list "$vendored"))"
added="$(LC_ALL=C comm -13 <(list "$upstream") <(list "$vendored"))"
if [ -n "$removed" ]; then
  fail "files of the published crate are missing: $removed"
fi
if [ "$added" != "$(printf '%s\n' ./LICENSE-APACHE ./LICENSE-MIT ./PATCHES.md)" ]; then
  fail "files other than the documented ones were added: $added"
fi

while IFS= read -r file; do
  if [ "$file" != ./src/lib.rs ] && ! cmp -s "$upstream/$file" "$vendored/$file"; then
    fail "$file differs from the published crate"
  fi
done < <(list "$upstream")

set +e
diff -u --label a/src/lib.rs --label b/src/lib.rs \
  "$upstream/src/lib.rs" "$vendored/src/lib.rs" > "$work/lib.rs.patch"
rc=$?
set -e
if [ "$rc" -ne 1 ]; then
  fail "diff of src/lib.rs found no change or failed (exit $rc)"
fi
if ! cmp -s "$work/lib.rs.patch" "$root/vendor/suppaftp.patch"; then
  diff -u "$root/vendor/suppaftp.patch" "$work/lib.rs.patch" || true
  fail "src/lib.rs differs from the published crate by more than vendor/suppaftp.patch"
fi

echo "$mit_sha256  $vendored/LICENSE-MIT" | sha256sum -c - \
  || fail "LICENSE-MIT is not upstream's"
echo "$apache_sha256  $vendored/LICENSE-APACHE" | sha256sum -c - \
  || fail "LICENSE-APACHE is not upstream's"

echo "vendor/suppaftp is suppaftp $version from crates.io plus vendor/suppaftp.patch"
