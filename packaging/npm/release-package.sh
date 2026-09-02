#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage:
  release-package.sh prepare <version> <arm64-bin> <x64-bin> <output-root>
  release-package.sh publish <package-name> <version> <tarball>
EOF
}

die() {
  printf 'npm release package: %s\n' "$1" >&2
  exit 1
}

validate_version() {
  local version="$1"
  [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]] || \
    die "version must use npm-compatible semver (received $version)"
}

validate_package_name() {
  local package_name="$1"
  [[ "$package_name" =~ ^(@[a-z0-9._~-]+/)?[a-z0-9._~-]+$ ]] || \
    die "invalid npm package name: $package_name"
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

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
pack_result=

cleanup_pack_result() {
  if [[ -n "${pack_result:-}" && -f "$pack_result" ]]; then
    rm -f -- "$pack_result"
  fi
}

prepare_package() {
  local version="$1"
  local arm64_binary="$2"
  local x64_binary="$3"
  local output_root="$4"
  local package_name package_directory package_json tarball_name tarball

  validate_version "$version"
  package_name="${NPM_PACKAGE_NAME:-}"
  [[ -n "$package_name" ]] || die 'NPM_PACKAGE_NAME is required for prepare'
  validate_package_name "$package_name"
  arm64_binary="$(canonical_file "$arm64_binary")"
  x64_binary="$(canonical_file "$x64_binary")"
  [[ -x "$arm64_binary" ]] || die "binary is not executable: $arm64_binary"
  [[ -x "$x64_binary" ]] || die "binary is not executable: $x64_binary"
  output_root="$(canonical_directory "$output_root")"
  package_directory="$output_root/remote-installer"

  NPM_PACKAGE_NAME="$package_name" node "$script_dir/build-package.mjs" \
    --version "$version" \
    --arm64-binary "$arm64_binary" \
    --x64-binary "$x64_binary" \
    --output "$package_directory" >&2

  package_json="$package_directory/package.json"
  [[ -f "$package_json" ]] || die "generated package metadata is missing: $package_json"
  NPM_PACKAGE_NAME="$package_name" EXPECTED_VERSION="$version" PACKAGE_JSON="$package_json" \
    node -e '
      const fs = require("fs");
      const pkg = JSON.parse(fs.readFileSync(process.env.PACKAGE_JSON, "utf8"));
      if (pkg.name !== process.env.NPM_PACKAGE_NAME || pkg.version !== process.env.EXPECTED_VERSION) {
        throw new Error(`npm metadata mismatch: ${pkg.name}@${pkg.version}`);
      }
    '

  pack_result="$(mktemp)"
  trap cleanup_pack_result EXIT
  npm pack "$package_directory" --pack-destination "$output_root" --json > "$pack_result"
  tarball_name="$(node -e '
    const fs = require("fs");
    const result = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    if (result.length !== 1 || !result[0].filename) {
      throw new Error("npm pack did not return exactly one tarball");
    }
    process.stdout.write(result[0].filename);
  ' "$pack_result")"
  [[ "$tarball_name" != */* && "$tarball_name" == *.tgz ]] || \
    die "npm pack returned an invalid tarball name: $tarball_name"
  tarball="$output_root/$tarball_name"
  [[ -f "$tarball" ]] || die "npm pack did not create the tarball: $tarball"

  # stdout is the interface consumed by the workflow; all other diagnostics use stderr.
  cleanup_pack_result
  trap - EXIT
  printf '%s\n' "$tarball"
}

publish_package() {
  local package_name="$1"
  local version="$2"
  local tarball="$3"
  local existing_version view_status

  validate_package_name "$package_name"
  validate_version "$version"
  tarball="$(canonical_file "$tarball")"
  [[ "$tarball" == *.tgz ]] || die "tarball must have a .tgz extension: $tarball"

  set +e
  existing_version="$(npm view "${package_name}@${version}" version \
    --registry https://registry.npmjs.org 2>&1)"
  view_status=$?
  set -e
  if [[ "$view_status" -eq 0 ]]; then
    if [[ "$existing_version" != "$version" ]]; then
      die "npm registry returned unexpected version: $existing_version"
    fi
    printf '%s@%s is already published; skipping npm publish\n' "$package_name" "$version" >&2
  elif [[ "$existing_version" == *E404* || "$existing_version" == *'404 Not Found'* ]]; then
    npm publish "$tarball" --access public
  else
    printf 'could not determine whether %s@%s exists; refusing to publish blindly\n' \
      "$package_name" "$version" >&2
    printf '%s\n' "$existing_version" >&2
    exit "$view_status"
  fi
}

if [[ "$#" -lt 1 ]]; then
  usage
  exit 2
fi

subcommand="$1"
shift
case "$subcommand" in
  prepare)
    [[ "$#" -eq 4 ]] || { usage; exit 2; }
    prepare_package "$@"
    ;;
  publish)
    [[ "$#" -eq 3 ]] || { usage; exit 2; }
    publish_package "$@"
    ;;
  -h|--help)
    usage
    ;;
  *)
    die "unknown subcommand: $subcommand"
    ;;
esac
