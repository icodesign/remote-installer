#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
template="$script_dir/remote-installer.rb.template"

: "${VERSION:?VERSION must be set to the release version (without the leading v)}"
: "${ARM64_SHA256:?ARM64_SHA256 must contain the arm64 asset SHA-256}"
: "${X86_64_SHA256:?X86_64_SHA256 must contain the x86_64 asset SHA-256}"
: "${HOMEBREW_GITHUB_REPOSITORY:?HOMEBREW_GITHUB_REPOSITORY must be owner/repository}"

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
  echo "render homebrew formula: invalid VERSION: $VERSION" >&2
  exit 1
fi
if [[ ! "$ARM64_SHA256" =~ ^[[:xdigit:]]{64}$ ]]; then
  echo "render homebrew formula: invalid ARM64_SHA256" >&2
  exit 1
fi
if [[ ! "$X86_64_SHA256" =~ ^[[:xdigit:]]{64}$ ]]; then
  echo "render homebrew formula: invalid X86_64_SHA256" >&2
  exit 1
fi
if [[ ! "$HOMEBREW_GITHUB_REPOSITORY" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "render homebrew formula: invalid HOMEBREW_GITHUB_REPOSITORY" >&2
  exit 1
fi

homepage="https://github.com/$HOMEBREW_GITHUB_REPOSITORY"
release_base_url="$homepage/releases/download/v$VERSION"
rendered="$(sed \
  -e "s|@@VERSION@@|$VERSION|g" \
  -e "s|@@ARM64_SHA256@@|$ARM64_SHA256|g" \
  -e "s|@@X86_64_SHA256@@|$X86_64_SHA256|g" \
  -e "s|@@HOMEPAGE@@|$homepage|g" \
  -e "s|@@RELEASE_BASE_URL@@|$release_base_url|g" \
  "$template")"

if [[ "$#" -gt 1 ]]; then
  echo "usage: render-formula.sh [output-file]" >&2
  exit 2
fi
if [[ "$#" -eq 1 ]]; then
  output="$1"
  mkdir -p "$(dirname -- "$output")"
  printf '%s\n' "$rendered" > "$output"
else
  printf '%s\n' "$rendered"
fi
