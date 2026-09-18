#!/usr/bin/env bash
# One-time setup for a clone: the pinned linters, the npm packages the web
# page and the docs site lint with, and the git hooks that run scripts/check.sh
# before each commit and push.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
scripts/tools.sh install
# The advisory database pre-commit checks against offline.
.tools/bin/cargo-deny fetch
npm --prefix web ci
npm --prefix site ci
git config core.hooksPath scripts/hooks
echo "hooks installed (core.hooksPath=scripts/hooks)"
