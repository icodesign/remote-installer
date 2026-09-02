#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage:
  github-release.sh stage <version> <asset-dir>
  github-release.sh publish <version>
EOF
}

die() {
  printf 'github release: %s\n' "$1" >&2
  exit 1
}

validate_version() {
  local version="$1"
  [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || \
    die "version must use X.Y.Z (received $version)"
}

require_github_environment() {
  [[ -n "${GITHUB_REPOSITORY:-}" ]] || die 'GITHUB_REPOSITORY is required'
  [[ -n "${GH_TOKEN:-}" ]] || die 'GH_TOKEN is required'
  [[ "$GITHUB_REPOSITORY" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || \
    die "invalid GITHUB_REPOSITORY: $GITHUB_REPOSITORY"
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
published_assets_directory=

cleanup_published_assets() {
  if [[ -n "${published_assets_directory:-}" && -d "$published_assets_directory" ]]; then
    rm -rf -- "$published_assets_directory"
  fi
}

stage_release() {
  local version="$1"
  local asset_dir="$2"
  local release_tag release_is_draft asset asset_name
  local assets

  validate_version "$version"
  require_github_environment
  [[ -d "$asset_dir" ]] || die "asset directory does not exist: $asset_dir"
  asset_dir="$(cd -- "$asset_dir" && pwd)"
  "$script_dir/release-assets.sh" verify "$asset_dir" "$version"

  release_tag="v${version}"
  assets=(
    "$asset_dir/remote-installer-${version}-darwin-arm64.tar.gz"
    "$asset_dir/remote-installer-${version}-darwin-arm64.tar.gz.sha256"
    "$asset_dir/remote-installer-${version}-darwin-x86_64.tar.gz"
    "$asset_dir/remote-installer-${version}-darwin-x86_64.tar.gz.sha256"
  )

  if release_is_draft="$(gh release view "$release_tag" \
    --repo "$GITHUB_REPOSITORY" \
    --json isDraft \
    --jq .isDraft 2>/dev/null)"; then
    if [[ "$release_is_draft" == "true" ]]; then
      gh release upload "$release_tag" --repo "$GITHUB_REPOSITORY" --clobber "${assets[@]}"
    else
      published_assets_directory="$(mktemp -d)"
      trap cleanup_published_assets EXIT

      gh release download "$release_tag" \
        --repo "$GITHUB_REPOSITORY" \
        --pattern "remote-installer-${version}-darwin-*.tar.gz*" \
        --dir "$published_assets_directory"
      for asset in "${assets[@]}"; do
        asset_name="${asset##*/}"
        if [[ ! -f "$published_assets_directory/$asset_name" ]] || \
          ! cmp -s "$asset" "$published_assets_directory/$asset_name"; then
          die "published release $release_tag does not match the rebuilt $asset_name"
        fi
      done
      cleanup_published_assets
      trap - EXIT
      printf 'release %s is already published with identical assets\n' "$release_tag" >&2
    fi
  else
    gh release create "$release_tag" \
      --repo "$GITHUB_REPOSITORY" \
      --verify-tag \
      --draft \
      --title "remote-installer ${version}" \
      --generate-notes \
      "${assets[@]}"
  fi
}

publish_release() {
  local version="$1"
  local release_tag release_is_draft

  validate_version "$version"
  require_github_environment
  release_tag="v${version}"
  release_is_draft="$(gh release view "$release_tag" \
    --repo "$GITHUB_REPOSITORY" \
    --json isDraft \
    --jq .isDraft)"
  case "$release_is_draft" in
    true)
      gh release edit "$release_tag" \
        --repo "$GITHUB_REPOSITORY" \
        --draft=false
      ;;
    false)
      printf 'release %s is already published\n' "$release_tag" >&2
      ;;
    *)
      die "GitHub returned an unexpected draft state for $release_tag: $release_is_draft"
      ;;
  esac
}

if [[ "$#" -lt 1 ]]; then
  usage
  exit 2
fi

subcommand="$1"
shift
case "$subcommand" in
  stage)
    [[ "$#" -eq 2 ]] || { usage; exit 2; }
    stage_release "$@"
    ;;
  publish)
    [[ "$#" -eq 1 ]] || { usage; exit 2; }
    publish_release "$@"
    ;;
  -h|--help)
    usage
    ;;
  *)
    die "unknown subcommand: $subcommand"
    ;;
esac
