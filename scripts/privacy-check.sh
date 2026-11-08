#!/usr/bin/env bash
# Privacy gate for commits.
#
# Blocks a commit when
#   1. the commit author e-mail is not a GitHub "noreply" address,
#   2. gitleaks finds a secret or a private network detail in the staged changes,
#   3. an added line, file name or the commit message matches a pattern from the
#      maintainer's local deny list.
#
# The deny list lives at .git/info/privacy-denylist. It is inside .git/ on
# purpose: it can never be committed, so personal strings (names, host names,
# home network ranges) stay on the maintainer's machine. One extended regular
# expression per line, matched case-insensitively; lines starting with # are
# comments.
#
# Usage:
#   scripts/privacy-check.sh staged          (pre-commit hook)
#   scripts/privacy-check.sh message <file>  (commit-msg hook)
#   scripts/privacy-check.sh tree            (scan all tracked files, e.g. before publishing)
set -euo pipefail

mode="${1:-staged}"
repo_root="$(git rev-parse --show-toplevel)"
denylist="$(git rev-parse --git-path info/privacy-denylist)"
status=0

fail() {
  printf 'privacy-check: %s\n' "$*" >&2
  status=1
}

load_patterns() {
  [[ -f "$denylist" ]] || return 0
  grep -vE '^[[:space:]]*(#|$)' "$denylist" || true
}

match_denylist() {
  # $1 = description, stdin = text to check
  local what="$1" text patterns hits
  patterns="$(load_patterns)"
  [[ -n "$patterns" ]] || return 0
  text="$(cat)"
  [[ -n "$text" ]] || return 0
  if hits="$(printf '%s\n' "$text" | grep -iE -f <(printf '%s\n' "$patterns"))"; then
    fail "deny-listed content in ${what}:"
    printf '%s\n' "$hits" | sed 's/^/    /' >&2
  fi
}

check_author() {
  local email
  email="$(git config user.email || true)"
  if [[ "$email" != *@users.noreply.github.com ]]; then
    fail "commit e-mail '${email}' is not a GitHub noreply address."
    fail "set it with: git config --local user.email '<id>+<login>@users.noreply.github.com'"
  fi
}

run_gitleaks() {
  if ! command -v gitleaks >/dev/null 2>&1; then
    fail "gitleaks is not installed; install it before committing (https://github.com/gitleaks/gitleaks)."
    return
  fi
  local args=(--no-banner --redact --config "$repo_root/.gitleaks.toml")
  case "$1" in
    staged) gitleaks protect --staged "${args[@]}" || fail "gitleaks reported findings in staged changes." ;;
    tree)
      # Only tracked files (with uncommitted changes), not build output or
      # other ignored files.
      local tmp
      tmp="$(mktemp -d)"
      (cd "$repo_root" && git ls-files -z | tar --null -T - -cf -) | tar -C "$tmp" -xf -
      gitleaks detect --no-git --source "$tmp" "${args[@]}" || fail "gitleaks reported findings in tracked files."
      rm -rf "$tmp"
      ;;
  esac
}

case "$mode" in
  staged)
    check_author
    run_gitleaks staged
    # Process substitution keeps match_denylist in this shell so it can set $status.
    match_denylist "staged file names" < <(git diff --cached --name-only --diff-filter=ACMR)
    match_denylist "staged changes" < <(
      git diff --cached -U0 --diff-filter=ACMR --no-color \
        | { grep -E '^\+' || true; } | { grep -vE '^\+\+\+ ' || true; } | sed 's/^+//'
    )
    ;;
  message)
    msg_file="${2:?commit message file required}"
    match_denylist "commit message" < <(grep -vE '^#' "$msg_file" || true)
    ;;
  tree)
    run_gitleaks tree
    match_denylist "tracked file names" < <(git ls-files)
    patterns="$(load_patterns)"
    if [[ -n "$patterns" ]]; then
      if hits="$(git ls-files -z | xargs -0 -r grep -InEi -f <(printf '%s\n' "$patterns") -- 2>/dev/null)"; then
        fail "deny-listed content in tracked files:"
        printf '%s\n' "$hits" | sed 's/^/    /' >&2
      fi
    fi
    ;;
  *)
    echo "usage: $0 {staged|message <file>|tree}" >&2
    exit 2
    ;;
esac

if [[ $status -ne 0 ]]; then
  printf 'privacy-check: commit blocked. Replace personal values with placeholders (see AGENTS.md).\n' >&2
fi
exit "$status"
