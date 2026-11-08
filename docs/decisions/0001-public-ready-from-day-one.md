# 0001 – Public-ready from day one

- Status: accepted
- Date: 2026-11-08

## Context

The repository starts private and is meant to become a public open-source
project later. Git history is permanent: anything committed now (addresses,
credentials, personal names, author e-mails) would be published together with
the history, or the history would have to be rewritten before release.

## Decision

1. Every commit must be publishable. Real infrastructure values never enter the
   repository; documentation ranges and example domains are used instead
   (see `AGENTS.md`).
2. Commits use the GitHub noreply e-mail address of the author.
3. A local commit hook (`scripts/privacy-check.sh`) blocks commits with
   secrets, private network details, non-noreply author e-mails, or strings
   from a per-maintainer deny list kept in `.git/info/` (outside version
   control).
4. CI runs gitleaks with the project rules in `.gitleaks.toml` on every push
   and pull request.
5. The license is chosen before the repository is made public. Until then the
   repository is "all rights reserved" by default.

## Consequences

- Contributors need `gitleaks` installed locally for the hook to pass.
- The initial scaffold commit was rewritten to the noreply address before
  the first push.
- Before going public: full-history gitleaks scan, `scripts/privacy-check.sh
  tree`, author/committer review, license file, `SECURITY.md`.
