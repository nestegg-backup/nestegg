# 0035 – Publish as a fresh repository in an organisation

- Status: accepted
- Date: 2026-11-13, carried out 2026-11-15

## Context

Before going public, the checks required by decision 0001 were run: gitleaks
over the full history of every remote branch and pull request head, the
local deny list, `scripts/privacy-check.sh tree`, the pull request texts and
comments, and a review of every commit author. No secrets, private network
details or personal data were found.

Eight commits from pull requests #2 and #5 have the author
`nestegg contributors <nestegg@users.noreply.github.com>`. That address was
made up while committing, but GitHub maps it to the unrelated user account
`nestegg` (decision 0004 already notes that the name is taken) and shows that
account as the author. The commit hook let it through because it accepted
any `*@users.noreply.github.com` address.

Rewriting `main` alone would not remove these commits: GitHub keeps the head
of every pull request (`refs/pull/<n>/head`) and they cannot be changed or
deleted by the repository owner.

Decision 0004 already says that the public repository lives in a dedicated
organisation instead of a personal account.

## Decision

1. The public repository is a new repository `nestegg-backup/nestegg`. Its
   history is the existing history with the author and committer of these
   commits set to the maintainer's
   `<id>+<login>@users.noreply.github.com` address. Test fixtures that look
   like real keys are replaced with obviously fake values in the whole
   history, not only in the current tree.
2. The new repository starts private. It is made public only after CI has
   passed on it and the checks from decision 0001 have been repeated on the
   rewritten history.
3. The old private repository stays private and keeps pull requests #1 to #5
   with their review discussions. It is not transferred.
4. The safety net is tightened: author and committer must use the
   `<id>+<login>@users.noreply.github.com` form (only that form names exactly
   one account), the hook reads the effective identity with `git var`, and CI
   checks the author and committer of every commit
   (`scripts/privacy-check.sh authors <range>`).
5. On the public repository, private vulnerability reporting is enabled
   (`SECURITY.md`) and workflow runs for pull requests from outside
   contributors need approval, because the rescue workflow runs long KVM jobs.

## Consequences

- Every commit hash from the first rewritten commit onwards changes. Local
  checkouts and worktrees are re-based onto the new history or cloned again.
- Links to pull requests #1 to #5 point to the private repository.
- Merges made in the GitHub web interface lose GitHub's signature when the
  history is rewritten.
- The first public CI run checks the author of every commit in the history.

## Timing (2026-11-15)

Decision 0034 tied publication to hub stages 1 and 2. The maintainer chose to
publish earlier, with hub stage 1 only, so that others can try it. The README
status "not released" stays, and the open points are listed in
[the roadmap](../roadmap.md).
