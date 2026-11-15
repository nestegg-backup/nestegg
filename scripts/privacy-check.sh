#!/usr/bin/env bash
# Privacy gate for commits.
#
# Blocks a commit when
#   1. the author or committer e-mail is not a GitHub "noreply" address of the
#      form <id>+<login>@users.noreply.github.com,
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
#   scripts/privacy-check.sh authors <range> (check author and committer of every commit, CI)
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

# Only the "<id>+<login>@users.noreply.github.com" form names exactly one
# GitHub account. A bare "<name>@users.noreply.github.com" may belong to an
# unrelated, older account of that name, which GitHub then shows as the author.
noreply_re='^[0-9]+\+[A-Za-z0-9-]+@users\.noreply\.github\.com$'

check_author() {
  # `git var` reports the identity the commit will really get, including
  # GIT_AUTHOR_EMAIL / GIT_COMMITTER_EMAIL and `git -c user.email=...`.
  local ident email
  for ident in GIT_AUTHOR_IDENT GIT_COMMITTER_IDENT; do
    email="$(git var "$ident" 2>/dev/null | sed -nE 's/^.*<([^>]*)>.*$/\1/p' || true)"
    if [[ ! "$email" =~ $noreply_re ]]; then
      fail "${ident} e-mail '${email}' is not a GitHub noreply address of the form <id>+<login>@users.noreply.github.com."
      fail "set it with: git config --local user.email '<id>+<login>@users.noreply.github.com'"
    fi
  done
}

check_history_authors() {
  # $1 = revision range. Merges done in the GitHub web interface are
  # committed by "GitHub <noreply@github.com>"; that committer is allowed.
  local sha author committer
  # An unknown revision must fail the check, not pass it as "no commits":
  # the loop below cannot see the exit status of `git log`.
  if ! git rev-list "$1" -- >/dev/null 2>&1; then
    fail "cannot read the revision range '$1'."
    return
  fi
  while IFS=$'\t' read -r sha author committer; do
    [[ "$author" =~ $noreply_re ]] \
      || fail "commit ${sha:0:12}: author e-mail '${author}' is not of the form <id>+<login>@users.noreply.github.com."
    [[ "$committer" =~ $noreply_re || "$committer" == "noreply@github.com" ]] \
      || fail "commit ${sha:0:12}: committer e-mail '${committer}' is not of the form <id>+<login>@users.noreply.github.com."
  done < <(git log --format='%H%x09%ae%x09%ce' "$1")
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
  authors)
    check_history_authors "${2:?revision range required}"
    ;;
  *)
    echo "usage: $0 {staged|message <file>|tree|authors <range>}" >&2
    exit 2
    ;;
esac

if [[ $status -ne 0 ]]; then
  printf 'privacy-check: commit blocked. Replace personal values with placeholders (see AGENTS.md).\n' >&2
fi
exit "$status"
