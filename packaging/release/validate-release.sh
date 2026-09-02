#!/usr/bin/env bash

set -euo pipefail

usage() {
  printf 'usage: %s <tag> <default-branch> <commit>\n' "${0##*/}" >&2
}

die() {
  printf 'validate release: %s\n' "$1" >&2
  exit 1
}

if [[ "$#" -ne 3 ]]; then
  usage
  exit 2
fi

tag="$1"
default_branch="$2"
commit="$3"

if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  die "release tags must use vX.Y.Z (received $tag)"
fi

if [[ -z "$default_branch" || "$default_branch" == *[[:space:]]* ]]; then
  die "default branch must be a non-empty ref name without whitespace"
fi

if [[ ! "$commit" =~ ^[0-9a-fA-F]{40}$ ]]; then
  die "commit must be a full 40-character Git object ID"
fi

version="${tag#v}"
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd -- "$script_dir/../.." && pwd)"
cargo_manifest="$repository_root/Cargo.toml"

[[ -f "$cargo_manifest" ]] || die "Cargo.toml was not found at repository root"

cargo_version="$(awk '
  /^\[package\][[:space:]]*$/ { in_package = 1; next }
  /^\[/ && in_package { exit }
  in_package && $1 == "version" && $2 == "=" {
    gsub(/"/, "", $3)
    print $3
    exit
  }
' "$cargo_manifest")"

[[ -n "$cargo_version" ]] || die "could not read [package].version from Cargo.toml"
[[ "$cargo_version" == "$version" ]] || die \
  "tag $tag does not match Cargo.toml package version $cargo_version"

cd -- "$repository_root"
remote_branch="refs/remotes/origin/$default_branch"
git show-ref --verify --quiet "$remote_branch" || \
  die "origin/$default_branch is not available in the local checkout"
# `--verify` validates a revision expression in `git rev-parse`; `cat-file`
# does not support that option for this check.
git rev-parse --verify --quiet "$commit^{commit}" >/dev/null 2>&1 || \
  die "commit $commit is not available in the local checkout"
git merge-base --is-ancestor "$commit" "$remote_branch" || \
  die "release tag $tag must point to a commit on $default_branch"

# stdout is the interface consumed by the workflow; diagnostics stay on stderr.
printf '%s\n' "$version"
