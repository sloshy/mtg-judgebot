#!/usr/bin/env bash
# The linters CI runs, at the versions CI runs them: the one list, used by
# .github/workflows/ci.yml and by scripts/check.sh (and so the git hooks).
#
#   scripts/tools.sh install [tool...]   download into .tools/ (default: all)
#   scripts/tools.sh path                print the directory to put on PATH
#
# Each tool is a prebuilt release binary from its GitHub releases page, kept
# under .tools/<tool>-<version>/ and linked from .tools/bin/, so bumping a
# version below installs the new one beside the old. Linux and macOS, x86_64
# and arm64. rustfmt/clippy come from rust-toolchain.toml, Biome from the
# npm lockfiles, sqlx-cli from `cargo install` (CONTRIBUTING.md).
set -euo pipefail

TYPOS=1.50.2
TAPLO=0.10.0
CARGO_DENY=0.20.2
CARGO_MACHETE=0.9.2
SHELLCHECK=0.11.0
ACTIONLINT=1.7.12
HADOLINT=2.15.1
LYCHEE=0.24.2

ALL=(typos taplo cargo-deny cargo-machete shellcheck actionlint hadolint lychee)

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Resolved physically: scripts/check.sh --at links a worktree's .tools here.
mkdir -p "$root/.tools"
tools="$(cd "$root/.tools" && pwd -P)"

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=darwin ;;
  *) echo "tools.sh: unsupported OS $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) echo "tools.sh: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

# Rust-style target triples, as typos/cargo-deny/cargo-machete/lychee name them.
if [ "$os" = linux ]; then
  musl="$arch-unknown-linux-musl"
  # cargo-machete publishes arm64 Linux as a glibc build only.
  machete_target=$([ "$arch" = aarch64 ] && echo aarch64-unknown-linux-gnu || echo "$musl")
else
  musl="$arch-apple-darwin"
  machete_target="$musl"
fi
go_arch=$([ "$arch" = x86_64 ] && echo amd64 || echo arm64)
hado_os=$([ "$os" = darwin ] && echo macos || echo linux)
hado_arch=$([ "$arch" = x86_64 ] && echo x86_64 || echo arm64)

# Prints "<version> <url> <kind>" for a tool; kind says how to unpack it.
spec() {
  case "$1" in
    typos) echo "$TYPOS https://github.com/crate-ci/typos/releases/download/v$TYPOS/typos-v$TYPOS-$musl.tar.gz tgz" ;;
    taplo) echo "$TAPLO https://github.com/tamasfe/taplo/releases/download/$TAPLO/taplo-$os-$arch.gz gz" ;;
    cargo-deny) echo "$CARGO_DENY https://github.com/EmbarkStudios/cargo-deny/releases/download/$CARGO_DENY/cargo-deny-$CARGO_DENY-$musl.tar.gz tgz" ;;
    cargo-machete) echo "$CARGO_MACHETE https://github.com/bnjbvr/cargo-machete/releases/download/v$CARGO_MACHETE/cargo-machete-v$CARGO_MACHETE-$machete_target.tar.gz tgz" ;;
    shellcheck) echo "$SHELLCHECK https://github.com/koalaman/shellcheck/releases/download/v$SHELLCHECK/shellcheck-v$SHELLCHECK.$os.$arch.tar.gz tgz" ;;
    actionlint) echo "$ACTIONLINT https://github.com/rhysd/actionlint/releases/download/v$ACTIONLINT/actionlint_${ACTIONLINT}_${os}_$go_arch.tar.gz tgz" ;;
    hadolint) echo "$HADOLINT https://github.com/hadolint/hadolint/releases/download/v$HADOLINT/hadolint-$hado_os-$hado_arch bin" ;;
    lychee) echo "$LYCHEE https://github.com/lycheeverse/lychee/releases/download/lychee-v$LYCHEE/lychee-$musl.tar.gz tgz" ;;
    *) echo "tools.sh: unknown tool $1" >&2; return 1 ;;
  esac
}

install_one() {
  local name=$1 version url kind dir tmp found
  read -r version url kind < <(spec "$name")
  dir="$tools/$name-$version"
  if [ ! -x "$dir/$name" ]; then
    echo "installing $name $version" >&2
    # Under .tools, so the final mv is a rename on one filesystem.
    tmp="$(mktemp -d "$tools/.download.XXXXXX")"
    # shellcheck disable=SC2064 # expand now: $tmp is this download's
    trap "rm -rf '$tmp'" EXIT
    curl -fsSL --retry 3 "$url" -o "$tmp/download"
    case "$kind" in
      tgz) tar -xzf "$tmp/download" -C "$tmp" ;;
      gz) gunzip -c "$tmp/download" > "$tmp/$name" ;;
      bin) mv "$tmp/download" "$tmp/$name" ;;
    esac
    # Archives put the binary at the top or one directory down.
    found="$(find "$tmp" -type f -name "$name" | head -n1)"
    if [ -z "$found" ]; then
      echo "tools.sh: no $name binary in $url" >&2
      rm -rf "$tmp"
      return 1
    fi
    chmod +x "$found"
    mkdir -p "$dir"
    # A rename is atomic, so a concurrent run sees no binary or a whole one.
    mv "$found" "$dir/$name"
    rm -rf "$tmp"
    trap - EXIT
  fi
  mkdir -p "$tools/bin"
  # Relinked only when it changes, so a concurrent run never finds it missing.
  if [ "$(readlink "$tools/bin/$name" 2> /dev/null)" != "$dir/$name" ]; then
    ln -sfn "$dir/$name" "$tools/bin/$name"
  fi
}

case "${1:-}" in
  install)
    shift
    if [ $# -eq 0 ]; then set -- "${ALL[@]}"; fi
    for t in "$@"; do install_one "$t"; done
    if [ -n "${GITHUB_PATH:-}" ]; then echo "$tools/bin" >> "$GITHUB_PATH"; fi
    ;;
  path) echo "$tools/bin" ;;
  *)
    echo "usage: scripts/tools.sh install [tool...] | path" >&2
    exit 2
    ;;
esac
