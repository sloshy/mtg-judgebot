#!/usr/bin/env bash
# The CI gates, run locally. The git hooks in scripts/hooks/ call this, and so
# can you:
#
#   scripts/check.sh [group...]              the working tree as it stands
#   scripts/check.sh --at <rev> [group...]   exactly <rev> (a commit or a tree)
#   scripts/check.sh --at <rev> --staged     the groups the index touches
#
# With no groups, every group runs. The hooks use --at: pre-commit checks the
# staged tree (`git write-tree`), pre-push each commit being pushed. That runs
# in a scratch worktree under .git/, so what is checked is exactly what is
# committed or pushed: no unstaged edit, untracked file or other branch's state
# can make it pass. The worktree shares target/ and .tools with this clone,
# keeps its own node_modules, and gets .env.example as its .env, as CI does.
#
# Groups:
#   rust   cargo fmt --check, clippy on every target with warnings denied
#   sqlx   migrations applied, then the committed .sqlx data matches the SQL
#   test   cargo test --workspace
#   web    Biome, then tsc + vite build
#   site   Biome, astro check, the build, lychee over its internal links
#   lint   cargo deny, cargo machete, taplo, typos, shellcheck, actionlint,
#          hadolint, docker compose config
# sqlx and test need Postgres (DATABASE_URL, else the one in .env).
#
# Every step in the chosen groups runs even when an earlier one fails, and the
# failures are listed at the end. Linters come from scripts/tools.sh.
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root" || exit 1

ALL_GROUPS=(rust sqlx test web site lint)
failed=()

bold() { printf '\033[1m%s\033[0m\n' "$*"; }

# DATABASE_URL from the environment, else from .env. Only that variable: the
# tests must see the environment CI gives them, not a developer's .env.
database_url() {
  if [ -z "${DATABASE_URL:-}" ] && [ -f "$1/.env" ]; then
    DATABASE_URL="$(sed -nE 's/^(export[[:space:]]+)?DATABASE_URL=//p' "$1/.env" | tail -n1)"
    DATABASE_URL="${DATABASE_URL%\"}"
    DATABASE_URL="${DATABASE_URL#\"}"
    DATABASE_URL="${DATABASE_URL%\'}"
    DATABASE_URL="${DATABASE_URL#\'}"
  fi
  if [ -n "${DATABASE_URL:-}" ]; then export DATABASE_URL; fi
}

# The groups a list of changed paths (one per line) touches, always with lint.
groups_for() {
  local rust=0 web=0 site=0 path
  while IFS= read -r path; do
    case "$path" in
      # Compiled in: include_str!/build.rs inputs and the sqlx offline data.
      crates/* | Cargo.toml | Cargo.lock | rust-toolchain.toml | .sqlx/* | data/* | judge.example.toml) rust=1 ;;
      web/*) web=1 ;;
      site/* | docs/* | README.md | CONTRIBUTING.md | CHANGELOG.md | SECURITY.md) site=1 ;;
      biome.json) web=1 site=1 ;;
    esac
  done
  if [ $rust = 1 ]; then echo rust; fi
  if [ $web = 1 ]; then echo web; fi
  if [ $site = 1 ]; then echo site; fi
  echo lint
}

# --at <rev> [args...]: check out <rev> in the scratch worktree and run that
# revision's own check.sh there (the gates as the pushed commit defines them).
run_at() {
  local rev=$1 common wt lock
  shift
  common="$(git rev-parse --path-format=absolute --git-common-dir)" || exit 1
  # Git exports these to hooks (GIT_INDEX_FILE to pre-commit, GIT_DIR from a
  # linked worktree). Left set, every `git -C "$wt"` below would act on the
  # caller's index and HEAD instead of the scratch worktree's.
  unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_PREFIX GIT_COMMON_DIR
  wt="$common/judgebot-check"
  lock="$common/judgebot-check.lock"
  if ! mkdir "$lock" 2> /dev/null; then
    echo "another check is using $wt (remove $lock if none is)" >&2
    exit 1
  fi
  # shellcheck disable=SC2064 # expand now: the paths are fixed
  trap "rmdir '$lock'" EXIT

  if [ ! -e "$wt/.git" ]; then
    git worktree prune
    git worktree add --detach --force -q "$wt" HEAD || exit 1
  fi
  if [ "$(git cat-file -t "$rev")" = tree ]; then
    git -C "$wt" checkout --detach --force -q HEAD || exit 1
    git -C "$wt" read-tree -u --reset "$rev" || exit 1
  else
    git -C "$wt" checkout --detach --force -q "$rev" || exit 1
  fi
  git -C "$wt" clean -fdq
  if [ -e "$root/.tools" ]; then ln -sfn "$root/.tools" "$wt/.tools"; fi
  # Its own node_modules (Vite resolves a symlinked one to a path outside the
  # project and the Astro build breaks), reinstalled only when the lockfile
  # changes. `git clean` above keeps them: they are ignored.
  local d stamp
  for d in web site; do
    # The lockfile's blob id in <rev>, a commit or a tree alike.
    stamp="$(git rev-parse "$rev:$d/package-lock.json" 2> /dev/null)"
    if [ -L "$wt/$d/node_modules" ]; then rm "$wt/$d/node_modules"; fi
    if [ "$(cat "$wt/$d/node_modules/.lock-stamp" 2> /dev/null)" != "$stamp" ]; then
      echo "npm ci in the check worktree's $d/" >&2
      npm --prefix "$wt/$d" ci --prefer-offline --no-audit --no-fund --loglevel=error > /dev/null || exit 1
      echo "$stamp" > "$wt/$d/node_modules/.lock-stamp"
    fi
  done
  if [ -f "$wt/.env.example" ]; then cp "$wt/.env.example" "$wt/.env"; fi
  if [ ! -x "$wt/scripts/check.sh" ]; then
    echo "$rev has no scripts/check.sh (it predates the gates); nothing to run" >&2
    exit 1
  fi

  database_url "$root"
  # One target dir: dependencies are built once for both trees.
  CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}" "$wt/scripts/check.sh" "$@"
}

if [ "${1:-}" = "--at" ]; then
  if [ -z "${2:-}" ]; then
    echo "usage: scripts/check.sh --at <rev> [--staged | group...]" >&2
    exit 2
  fi
  rev="$2"
  shift 2
  if [ "${1:-}" = "--staged" ]; then
    # Deletions and both sides of a rename count: removing a file can break
    # the build as surely as editing one.
    groups=()
    while IFS= read -r g; do
      groups+=("$g")
    done < <(git diff --cached --name-only --no-renames --diff-filter=ACMRD | groups_for)
    set -- "${groups[@]}"
    # A commit is checked offline where it can be; pre-push fetches.
    export CHECK_OFFLINE=1
  fi
  run_at "$rev" "$@"
  exit $?
fi

# step <name> <command...>: run it, and remember the name if it fails.
step() {
  local name=$1
  shift
  bold "==> $name"
  if ! "$@"; then
    failed+=("$name")
  fi
}

need_node_modules() {
  if [ ! -d "$1/node_modules" ]; then
    echo "$1/node_modules is missing: run \`npm --prefix $1 ci\`" >&2
    return 1
  fi
}

need_database() {
  database_url "$root"
  if [ -z "${DATABASE_URL:-}" ]; then
    echo "DATABASE_URL is not set (see .env.example)" >&2
    return 1
  fi
  # A quick reachability probe for the common host:port form; anything it
  # cannot parse (a socket, IPv6, ?host=) is left for cargo to report.
  local rest host port
  rest="${DATABASE_URL#*://}"
  rest="${rest##*@}"
  rest="${rest%%[/?]*}"
  host="${rest%:*}"
  port="${rest##*:}"
  if [ "$port" = "$rest" ]; then port=5432; fi
  case "$host" in "" | *[\[\]]*) return 0 ;; esac
  if ! (exec 3<> "/dev/tcp/$host/$port") 2> /dev/null; then
    echo "Postgres is not reachable at $host:$port: \`docker compose up -d db\`" >&2
    return 1
  fi
}

tools_ready() {
  scripts/tools.sh install > /dev/null || return 1
  PATH="$(scripts/tools.sh path):$PATH"
  export PATH
}

group_rust() {
  step "cargo fmt" cargo fmt --all --check
  # `-D warnings` as a clippy argument rather than RUSTFLAGS, which would
  # change every crate's fingerprint and rebuild the world for `cargo build`.
  step "cargo clippy" env SQLX_OFFLINE=true cargo clippy --workspace --all-targets --quiet -- -D warnings
}

group_sqlx() {
  if ! need_database; then
    failed+=("sqlx (no database)")
    return
  fi
  if ! cargo sqlx --version > /dev/null 2>&1; then
    echo "sqlx-cli is missing: see CONTRIBUTING.md" >&2
    failed+=("sqlx (no sqlx-cli)")
    return
  fi
  # As CI does, against a database built from this tree's migrations alone:
  # judgebot_check on the same server, dropped and re-created each run, so a
  # branch's migration never lands in the development database.
  local base query check
  base="${DATABASE_URL%%\?*}"
  query="${DATABASE_URL#"$base"}"
  check="${base%/*}/judgebot_check$query"
  step "migrations (judgebot_check)" env DATABASE_URL="$check" \
    cargo sqlx database reset -y --source crates/bot/migrations
  step ".sqlx is in sync" env DATABASE_URL="$check" \
    cargo sqlx prepare --check --workspace -- --all-targets
}

group_test() {
  if ! need_database; then
    failed+=("test (no database)")
    return
  fi
  # Offline, like clippy: the queries compile against the committed .sqlx data
  # (which the sqlx group proves current), not against whatever schema
  # DATABASE_URL happens to hold. CI's database is empty, and compiling online
  # there fails every query. The tests still need the server: #[sqlx::test]
  # creates and migrates its own throwaway databases off DATABASE_URL.
  step "cargo test" env SQLX_OFFLINE=true cargo test --workspace --quiet
}

group_web() {
  if ! need_node_modules web; then
    failed+=("web (no node_modules)")
    return
  fi
  step "web: biome" npm --prefix web run --silent lint
  step "web: tsc + build" npm --prefix web run --silent build
}

group_site() {
  if ! need_node_modules site; then
    failed+=("site (no node_modules)")
    return
  fi
  step "site: biome" npm --prefix site run --silent lint
  step "site: astro check" npm --prefix site run --silent check
  step "site: build" npm --prefix site run --silent build -- --silent
  # The pages link to <base>/…, so lychee's root holds dist under the base's
  # name (the one the build used: SITE_BASE, else /mtg-judgebot).
  local base links
  base="${SITE_BASE:-/mtg-judgebot}"
  base="/${base#/}"
  base="${base%/}"
  links="$(mktemp -d)"
  if [ -z "$base" ]; then
    ln -s "$root/site/dist" "$links/root"
  else
    mkdir -p "$links/root$(dirname "$base")"
    ln -s "$root/site/dist" "$links/root$base"
  fi
  step "site: links" lychee --offline --include-fragments --index-files index.html \
    --root-dir "$links/root" --no-progress 'site/dist/**/*.html'
  rm -rf "$links"
}

group_lint() {
  # A commit uses the advisory database already fetched (pre-push refreshes
  # it), so committing works offline.
  # A clone that never fetched it fetches once here.
  if [ "${CHECK_OFFLINE:-0}" = 1 ] && [ -d "${CARGO_HOME:-$HOME/.cargo}/advisory-dbs" ]; then
    step "cargo deny" cargo deny --offline --log-level error check
  else
    step "cargo deny" cargo deny --log-level error check
  fi
  step "cargo machete" cargo machete
  step "taplo" env RUST_LOG=warn taplo fmt --check
  step "typos" typos
  step "shellcheck" shellcheck scripts/*.sh scripts/hooks/*
  step "actionlint" actionlint
  step "hadolint" hadolint Dockerfile
  # docker-compose.yml names .env as an env_file. The --at worktree has the
  # example there, as CI's compose job does; a plain run uses yours.
  if ! command -v docker > /dev/null; then
    echo "docker not found: compose config not checked" >&2
  elif [ ! -f .env ]; then
    echo "no .env: compose config not checked" >&2
  else
    step "compose config" docker compose config --quiet
  fi
}

if [ $# -gt 0 ]; then
  groups=("$@")
else
  groups=("${ALL_GROUPS[@]}")
fi
for g in "${groups[@]}"; do
  case "$g" in
    rust | sqlx | test | web | site | lint) ;;
    *)
      echo "unknown group: $g (one of: ${ALL_GROUPS[*]})" >&2
      exit 2
      ;;
  esac
done

if ! tools_ready; then
  echo "could not install the linters (scripts/tools.sh install)" >&2
  exit 1
fi

for g in "${groups[@]}"; do
  "group_$g"
done

if [ ${#failed[@]} -gt 0 ]; then
  echo
  bold "failed:"
  printf '  %s\n' "${failed[@]}"
  exit 1
fi
bold "all checks passed (${groups[*]})"
