#!/usr/bin/env bash
# Check the version a release reports.
#
#   scripts/release-version.sh          every copy of the version agrees
#   scripts/release-version.sh vX.Y.Z   and it is the tag's, with its changelog
#
# The crates take theirs from `[workspace.package]` in Cargo.toml, so they
# cannot disagree with each other. The web app and the site carry their own
# in package.json and package-lock.json, bumped by hand with it.
#
# With a tag (publish-image.yml, before anything is published) the version
# must also be the tag's, and CHANGELOG.md must have the tag's `## [X.Y.Z]`
# heading. A pre-release tag (`v2.0.0-rc.1`) needs the tree at exactly that
# version, since the binaries report it and a pre-release works only with its
# own build (judge_core::release), and no heading of its own. A tag with
# build metadata (`+…`) is refused.
#
# A patch version (X.Y.Z with Z above 0, its `-dev` included) must have the
# migrations of `vX.Y.0`, none added, edited, renamed or deleted: a patch release is the one kind that is safe to
# roll back (CONTRIBUTING, Rolling back; docs/DECISIONS.md D45). Without that
# tag in the clone the check is skipped with a note, except when releasing,
# where it fails: fetch the tags.
# Run from the repository root. Needs jq.

set -euo pipefail

version="$(sed -n '/^\[workspace.package\]$/,/^\[/{s/^version = "\(.*\)"$/\1/p;}' Cargo.toml)"
if [ -z "$version" ]; then
  echo "Cargo.toml has no [workspace.package] version" >&2
  exit 1
fi

ok=0
for dir in web site; do
  for found in \
    "$dir/package.json:$(jq -r .version "$dir/package.json")" \
    "$dir/package-lock.json:$(jq -r .version "$dir/package-lock.json")" \
    "$dir/package-lock.json packages[\"\"]:$(jq -r '.packages[""].version' "$dir/package-lock.json")"; do
    if [ "${found##*:}" != "$version" ]; then
      echo "${found%:*} is ${found##*:}, but Cargo.toml's [workspace.package] is $version" >&2
      ok=1
    fi
  done
done

# The migrations a patch version adds, which must be none. `RELEASE_VERSION`
# stands in for the tree's version so the check can be tried on any version.
checked="${RELEASE_VERSION:-$version}"
core="${checked%%-*}"
patch="${core##*.}"
base="v${core%.*}.0"
if [ "$patch" != 0 ]; then
  if git rev-parse -q --verify "refs/tags/$base" >/dev/null 2>&1; then
    # Against the files on disk, not a commit: that is what sqlx embeds, and
    # what every caller has (a staged tree in a scratch worktree included).
    # Any difference counts. A renamed migration is a new one to sqlx, and an
    # edited one fails its checksum.
    changed="$(
      git diff --name-status --no-renames "$base" -- crates/bot/migrations
      git ls-files --others --exclude-standard -- crates/bot/migrations | sed 's/^/?	/'
    )"
    if [ -n "$changed" ]; then
      echo "version $checked is a patch release, but its migrations differ from $base (A added, M edited, D deleted, ? untracked):" >&2
      echo "  ${changed//$'\n'/$'\n'  }" >&2
      echo "a patch release changes no migration (CONTRIBUTING, Rolling back): make it the next minor" >&2
      ok=1
    fi
  elif [ $# -gt 0 ]; then
    echo "cannot check that patch release $checked adds no migration: tag $base is not in this clone (git fetch --tags)" >&2
    ok=1
  else
    echo "note: tag $base is not in this clone, so the no-migration check for patch version $checked was skipped" >&2
  fi
fi

if [ $# -eq 0 ]; then
  exit "$ok"
fi

tag="$1"
tagged="${tag#v}"
release="${tagged%%-*}"
if [ "$tag" = "$tagged" ] || [[ "$tag" == *+* ]]; then
  echo "release tag '$tag' is not vX.Y.Z or vX.Y.Z-pre (no +build metadata)" >&2
  exit 1
fi
if [ "$version" != "$tagged" ]; then
  echo "release tag '$tag' but the crates are version $version: bump [workspace.package] in Cargo.toml (CONTRIBUTING, Releases), then delete this release and its tag and create them again" >&2
  ok=1
fi
if [ "$tagged" = "$release" ] && ! grep -qE "^## \[${release//./\\.}\]( |$)" CHANGELOG.md; then
  echo "CHANGELOG.md has no '## [$release]' heading: move the Unreleased section under it" >&2
  ok=1
fi
exit "$ok"
