#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage:
  release-assets.sh package <binary> <version> <arch> <output-dir>
  release-assets.sh verify <asset-dir> <version>
  release-assets.sh extract <asset-dir> <version> <output-dir>
EOF
}

die() {
  printf 'release assets: %s\n' "$1" >&2
  exit 1
}

validate_version() {
  local version="$1"
  [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || \
    die "version must use X.Y.Z (received $version)"
}

validate_arch() {
  case "$1" in
    arm64|x86_64) ;;
    *) die "architecture must be arm64 or x86_64 (received $1)" ;;
  esac
}

canonical_file() {
  local candidate="$1"
  [[ -n "$candidate" && -f "$candidate" ]] || die "file does not exist: $candidate"
  local parent
  parent="$(cd -- "$(dirname -- "$candidate")" && pwd)" || die "cannot resolve file path: $candidate"
  printf '%s/%s\n' "$parent" "$(basename -- "$candidate")"
}

canonical_directory() {
  local candidate="$1"
  [[ -n "$candidate" ]] || die "directory path must not be empty"
  mkdir -p -- "$candidate"
  [[ -d "$candidate" ]] || die "not a directory: $candidate"
  cd -- "$candidate" && pwd
}

sha256_file() {
  local file="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{ print $1 }'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{ print $1 }'
  else
    die "neither shasum nor sha256sum is available"
  fi
}

asset_path() {
  local asset_dir="$1"
  local version="$2"
  local arch="$3"
  printf '%s/remote-installer-%s-darwin-%s.tar.gz\n' "$asset_dir" "$version" "$arch"
}

checksum_path() {
  printf '%s.sha256\n' "$1"
}

staging_dir=
cleanup_staging() {
  if [[ -n "${staging_dir:-}" && -d "$staging_dir" ]]; then
    rm -rf -- "$staging_dir"
  fi
}

verify_archive_contents() {
  local asset="$1"
  local entries
  entries="$(tar -tzf "$asset")"
  [[ "$entries" == "remote-installer" ]] || \
    die "archive contains unexpected entries: $asset"
}

verify_assets() {
  local asset_dir="$1"
  local version="$2"
  local arch asset checksum expected_digest expected_name actual_digest

  validate_version "$version"
  [[ -d "$asset_dir" ]] || die "asset directory does not exist: $asset_dir"
  asset_dir="$(cd -- "$asset_dir" && pwd)"

  for arch in arm64 x86_64; do
    asset="$(asset_path "$asset_dir" "$version" "$arch")"
    checksum="$(checksum_path "$asset")"
    [[ -f "$asset" ]] || die "missing release asset: $asset"
    [[ -f "$checksum" ]] || die "missing checksum file: $checksum"

    expected_digest="$(awk 'NF { print $1; exit }' "$checksum" | tr '[:upper:]' '[:lower:]')"
    expected_name="$(awk 'NF { print $2; exit }' "$checksum")"
    [[ "$expected_digest" =~ ^[0-9a-f]{64}$ ]] || \
      die "invalid checksum in $checksum"
    [[ "$expected_name" == "$(basename -- "$asset")" ]] || \
      die "checksum filename does not match $asset"

    actual_digest="$(sha256_file "$asset")"
    [[ "$actual_digest" == "$expected_digest" ]] || \
      die "checksum mismatch: $asset"
    verify_archive_contents "$asset"
  done

  printf 'verified release assets for %s in %s\n' "$version" "$asset_dir" >&2
}

package_asset() {
  local binary="$1"
  local version="$2"
  local arch="$3"
  local output_dir="$4"
  local asset checksum digest

  validate_version "$version"
  validate_arch "$arch"
  binary="$(canonical_file "$binary")"
  [[ -x "$binary" ]] || die "binary is not executable: $binary"
  output_dir="$(canonical_directory "$output_dir")"

  asset="$(asset_path "$output_dir" "$version" "$arch")"
  checksum="$(checksum_path "$asset")"
  staging_dir="$(mktemp -d)"
  trap cleanup_staging EXIT

  cp "$binary" "$staging_dir/remote-installer"
  chmod 755 "$staging_dir/remote-installer"
  touch -t 198001010000 "$staging_dir/remote-installer"
  (
    cd -- "$staging_dir"
    COPYFILE_DISABLE=1 tar \
      --uid 0 \
      --gid 0 \
      --uname root \
      --gname wheel \
      -cf - \
      remote-installer
  ) | gzip -n > "$asset"

  [[ -s "$asset" ]] || die "archive was not created: $asset"
  digest="$(sha256_file "$asset")"
  [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || die "could not calculate SHA-256: $asset"
  printf '%s  %s\n' "$digest" "$(basename -- "$asset")" > "$checksum"
  [[ -s "$checksum" ]] || die "checksum file was not created: $checksum"
  cleanup_staging
  trap - EXIT
  printf 'created %s and %s\n' "$asset" "$checksum" >&2
}

extract_assets() {
  local asset_dir="$1"
  local version="$2"
  local output_dir="$3"
  local arch asset destination

  validate_version "$version"
  [[ -d "$asset_dir" ]] || die "asset directory does not exist: $asset_dir"
  asset_dir="$(cd -- "$asset_dir" && pwd)"
  verify_assets "$asset_dir" "$version"
  output_dir="$(canonical_directory "$output_dir")"

  for arch in arm64 x86_64; do
    asset="$(asset_path "$asset_dir" "$version" "$arch")"
    destination="$output_dir/$arch"
    mkdir -p -- "$destination"
    tar -xzf "$asset" -C "$destination"
    [[ -x "$destination/remote-installer" ]] || \
      die "extracted binary is missing or not executable: $destination/remote-installer"
  done

  printf 'extracted release assets for %s into %s\n' "$version" "$output_dir" >&2
}

if [[ "$#" -lt 1 ]]; then
  usage
  exit 2
fi

subcommand="$1"
shift
case "$subcommand" in
  package)
    [[ "$#" -eq 4 ]] || { usage; exit 2; }
    package_asset "$@"
    ;;
  verify)
    [[ "$#" -eq 2 ]] || { usage; exit 2; }
    verify_assets "$@"
    ;;
  extract)
    [[ "$#" -eq 3 ]] || { usage; exit 2; }
    extract_assets "$@"
    ;;
  -h|--help)
    usage
    ;;
  *)
    die "unknown subcommand: $subcommand"
    ;;
esac
