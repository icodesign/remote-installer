#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
: "${VERSION:?VERSION must be set to the release version (without the leading v)}"
: "${ARM64_SHA256:?ARM64_SHA256 must contain the arm64 asset SHA-256}"
: "${X86_64_SHA256:?X86_64_SHA256 must contain the x86_64 asset SHA-256}"
: "${HOMEBREW_GITHUB_REPOSITORY:?HOMEBREW_GITHUB_REPOSITORY must be owner/repository}"
: "${HOMEBREW_TAP_REPOSITORY:?HOMEBREW_TAP_REPOSITORY must be the owner/repository of the tap}"
: "${HOMEBREW_TAP_TOKEN:?HOMEBREW_TAP_TOKEN must provide write access to the tap repository}"

if [[ ! "$HOMEBREW_TAP_REPOSITORY" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "update homebrew tap: invalid HOMEBREW_TAP_REPOSITORY" >&2
  exit 1
fi

tap_directory="${HOMEBREW_TAP_DIRECTORY:-$(mktemp -d)}"
if [[ -e "$tap_directory/.git" ]]; then
  if [[ -n "$(git -C "$tap_directory" status --porcelain)" ]]; then
    echo "update homebrew tap: existing tap checkout has uncommitted changes" >&2
    exit 1
  fi
  tap_branch="$(git -C "$tap_directory" symbolic-ref --quiet --short HEAD)" || {
    echo "update homebrew tap: existing tap checkout is detached" >&2
    exit 1
  }
  git -C "$tap_directory" fetch --prune origin "$tap_branch"
  git -C "$tap_directory" merge --ff-only "origin/$tap_branch"
else
  git clone --quiet \
    "https://x-access-token:${HOMEBREW_TAP_TOKEN}@github.com/${HOMEBREW_TAP_REPOSITORY}.git" \
    "$tap_directory"
  tap_branch="$(git -C "$tap_directory" symbolic-ref --quiet --short HEAD)"
fi

formula_path="$tap_directory/Formula/remote-installer.rb"
mkdir -p "$(dirname -- "$formula_path")"
VERSION="$VERSION" \
ARM64_SHA256="$ARM64_SHA256" \
X86_64_SHA256="$X86_64_SHA256" \
HOMEBREW_GITHUB_REPOSITORY="$HOMEBREW_GITHUB_REPOSITORY" \
  "$script_dir/render-formula.sh" "$formula_path"

# Stage before checking the diff so a newly created formula is handled as a
# change too; `git diff` alone does not report untracked files.
git -C "$tap_directory" add Formula/remote-installer.rb
if git -C "$tap_directory" diff --cached --quiet -- Formula/remote-installer.rb; then
  echo "Homebrew formula is already current for remote-installer $VERSION"
  exit 0
fi

git -C "$tap_directory" \
  -c user.name='remote-installer release bot' \
  -c user.email='41898282+github-actions[bot]@users.noreply.github.com' \
  commit -m "remote-installer $VERSION"
git -C "$tap_directory" push origin "HEAD:$tap_branch"
