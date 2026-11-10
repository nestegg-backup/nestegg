# 0022 – Settings changes that reduce the protection

- Status: accepted
- Date: 2026-11-10

## Context

Members of group `nestegg` may change a fixed set of settings through the web
UI (`PATCH /api/v1/config`, [decision 0013](0013-editable-settings-in-the-web-ui.md)):
the backup interval, "overdue after", the exclude list, Wake-on-LAN and the
backup timer. That is convenient, but it also lets a group member — or a
compromised browser session, or a less trusted member of the household — turn
off the protection of this computer's backups quietly:

- Automatic backups off **and** a very long "overdue after" gave weeks without
  any message: the dashboard would say "overdue" eventually, but nobody looks
  at it.
- An exclude such as `/home/alice` or (in system mode) `/etc` was accepted, and
  the run still counted as "complete", so whole home directories or system
  directories could silently drop out of the backup.
- Only the journal recorded who changed what, and nobody reads the journal.

The owner of a family computer should notice when its protection is lowered,
and the most destructive settings should be out of a group member's reach
entirely.

## Decision

Three parts, on top of the existing editable-settings machinery, plus the
delivery, flood-control, locking and validation rules that make them hold up.

### 1. Report every protection-reducing change made through the API

After a successful `PATCH /config` by a group member, the daemon classifies the
change (`settings::reductions`). A change **lowers the protection** when it
switches automatic backups off, adds an exclude pattern, removes a `!`
re-include, changes the order of the patterns that stay while the list has a
`!` re-include (later patterns win, so the order can decide what is kept; this
is reported whenever the order changes, without working out the effect), raises
"overdue after", raises the interval, or switches Wake-on-LAN off. Changes that
raise the protection (more frequent backups, a lower threshold, a removed
exclude, an added re-include, Wake-on-LAN on) are not reported.

A notification event, **`settings`**, carries this to the logged-in users and
the hook: who changed what, with plain German and English text like the other
events. Delivery reuses the health check's machinery (`notify.rs`). A change
made by editing the file as **root** is not reported — root is trusted and does
not go through the API; a change made through the API by root is not reported
either.

### 2. "Automatic backups are off" is a condition of its own

A switched-off backup timer, while nestegg is configured, is its own
health-check condition (`automatic_off`), announced at once and repeated every
seven days, **independent of the overdue threshold**. It replaces the old
"overdue while the timer is off" variant; the text still says how old the last
successful backup is. It is evaluated next to the other conditions, so it is
announced even when the last run also failed. A deliberately paused computer is
thus reported calmly (weekly), but from the start, not only after weeks.

### 3. The API refuses the extremes; root keeps them in the file

- **"Overdue after"** is capped through the API at `max(168, interval + 72)`
  hours — seven days, or the interval plus three days when that is larger, so a
  weekly rhythm (interval 168 h) can still be overdue after up to ten days. The
  cap is evaluated against the interval in effect *after the same request*,
  since both keys can change together. A larger value that root set in the file
  is kept and shown, and can then only be *lowered* through the API, never
  raised past what the file already has. `limits.overdue_after_hours` reports
  `base` (168), `per_interval_extra` (72) and `file_minimum` so the web
  interface can recompute the cap for an interval it is about to set.

- **Excludes** are judged as the backup client really applies them. Each
  `--exclude` is matched *inside each archive*, relative to that archive's root
  and with later patterns winning (a `!` re-include can bring a path back). The
  archives that are actually produced decide what is tested:

  - in normal mode each configured source is its own archive (`home.pxar` from
    `/home`, with entries `/alice`, `/bob`, …): a change is refused if its whole
    new list would empty a source or a regular user's home directory inside it
    (`/alice`, `alice/**`, `/[a-z]*`, …);
  - in system mode the operating system is one `system.pxar` rooted at `/`: the
    top-level system directories (`/etc /home /root /usr /var /opt /boot /srv`)
    and the home directories by their full path (`/home/alice`) are tested
    there; a source that keeps its own archive (a data disk that must be its own
    mount) is tested separately.

  "Would empty the folder" means the list excludes the folder entry itself,
  excludes any **ancestor directory** of it inside the archive (a pattern that
  takes out a parent — `/var/home` above the home `/var/home/alice` — takes out
  everything below it), or excludes **every child**. The folder-entry and the
  ancestor tests are pure computation. The children are the folder's **real
  top-level entry names**, and here is the one place the guard looks at the file
  system. It reads them **with the identity of the user who sent the request**,
  never as root: the peer credentials of the socket connection (uid and primary
  gid, no supplementary groups), in a child process dropped to that identity
  with a time limit, so what the requester could not list themselves counts as
  "entries unknown". The guard's answer therefore never depends on a name the
  requester cannot see anyway — a group member cannot learn from it which files
  exist in another user's `0700` home directory or in `/root`. The read is
  bounded: names only, at most 4096 of them, a two-second limit for one folder
  and **one deadline of four seconds for all the reads of a request together**
  (the clock starts with the first read; a folder that is not read by then
  counts as unknown, and no read is started after it), so a request holds the
  settings lock for a few seconds at most, however many home directories there
  are. After the limit the child is killed and reaped without blocking the
  request: the read waits for data with `poll` on the request's own thread, so
  no reader thread can be left behind, and waiting for the child to end has the
  same deadline — a child that closed its output but does not die (one stuck in
  uninterruptible sleep on a dead file system) is reaped on a detached thread
  that keeps its slot until it is gone. No more than a fixed number of such
  reads run at once: a further read while that bound is reached counts the
  folder as unknown rather than waiting, so a reader hung on a dead file system
  cannot make requests pile up readers without end. The child runs a standard tool (`find`, part of the
  required `findutils` package on Debian/Ubuntu), not nestegg code; a missing
  tool is handled as "unknown". A protected folder is read by the path the
  configuration (or the account database) gives, and `find` does not follow a
  symbolic link at that path: such a folder counts as unknown and is judged by
  computation and made-up names. That matches the backup client, which does not
  follow a link either (`pxar create`, the archive code the client uses, refuses
  a link as the archive root with `ELOOP`, so a source that is a link cannot be
  backed up at all, and a link inside a source is stored as a link with nothing
  below it): the entries behind a link are not in the archive and must not
  decide anything. The protected folders are not resolved before the read
  (resolving would run as root and could depend on what the requester cannot
  see). For a folder with more than 4096 entries the
  first 4096 decide, so such a list can be refused although a later entry would
  stay; that errs on the side of the guard. When the real names are not known
  (the folder is empty, the requester cannot read it, it does not answer in
  time, or the requester is not known on the code path) made-up names are used
  instead, one per kind of first character (digit, upper and lower case letter,
  underscore, dot, non-ASCII), and the folder counts as emptied when the list
  leaves out all of them. A change is refused only when the **new** list would
  empty a folder that the **old** list did not, so a dangerous pattern root
  already put in the file is grandfathered and a plain reorder is refused only
  when it changes the outcome.

  The folder guard is the **last validation of the request's values**. Everything
  that can refuse a request for another reason — a value out of range, an
  invalid pattern, Wake-on-LAN without a `[wake]` section — is checked before
  any folder is read. A refusal for another reason therefore never depends on
  folder contents, so it cannot be combined with the folder read to probe for
  names while leaving no trace (the earlier order read the folder first, then
  refused for the other reason, which told the requester whether a name existed
  without writing or reporting anything). Steps do follow it — rewriting the
  file text, parsing the result with `Config::parse` and checking that the
  change landed where the configuration reads it — and they can still refuse a
  request in principle (a configuration that does not parse after the change).
  None of them looks at folder contents; they act on the request's values and
  the file text, which the checks before it already cover, so they are safety
  nets that a request that passed the guard does not trip in practice. If one
  did, the refusal changes nothing and its message carries no folder
  information; the requester only learns that the guard let the request pass,
  and the guard reads with the requester's own identity, so that tells them
  about folders they can list themselves anyway. Writing the file and switching
  the timer come after that and are failures of carrying out an accepted
  request, not refusals.

  A pattern the matcher cannot parse is refused up front with a plain message:
  an unclosed `[`…`]` character class, a trailing `\`, a malformed range
  (`[a-]`). nestegg links an older release (0.1.2) of the client's own
  `pathpatterns` crate (1.0.0); both share an off-by-one bug that *panics* the
  parser on an unclosed class, so the parse is wrapped and any panic is caught
  and turned into a refusal rather than an HTTP 500.

The refusal messages say in plain words why, and that an administrator can set
the value as root in `/etc/nestegg/config.toml`.

### 4. Durable delivery, no flood, honest about failures

- **Write-ahead report.** Before the daemon changes the configuration it records
  a short *intent* in a small inbox (`notify-settings.json`, its own lock). If
  even the intent cannot be recorded the change is not applied. After the change
  the intent is completed with the real reductions (or dropped when nothing was
  lowered). A crash in between still leaves the intent, which the next check
  delivers as a generic "the settings were changed by …; the details are in the
  system log" message. A damaged inbox is set aside under a `.corrupt-…` name,
  logged, and reported as such — never silently discarded. The HTTP answer is
  not "200 OK" unless the report was durably recorded.

- **Delivered after a short quiet period, and again at the end of the run.**
  After recording the daemon starts `nestegg-notify.service` (`systemctl start
  --no-block`). The check waits for the quiet period described under "No flood"
  (about 20 seconds after the last change, at most about two minutes), drains
  the inbox under its main lock and announces it. Before it releases the lock
  it drains the inbox *again* (at most five times, as long as something new was
  moved), so a change recorded while the check was running — its hook can be
  slow — is delivered by that same run instead of waiting for the hourly timer.
  An intent whose request is still being handled (younger than a minute) is
  left in the inbox by a check: the daemon completes or cancels it in a moment,
  and it must not go out as a generic message first.

- **No flood.** A check started for a settings change waits out a short quiet
  period before it takes the main lock (until the newest pending change is about
  20 s old, at most ~2 minutes from the oldest), so a burst coalesces into one
  run; systemd collapses the repeated `start --no-block` triggers onto that one
  sleeping instance (`StartLimitIntervalSec=0`, kept so a burst cannot trip the
  default start-rate limit and drop reports). Everything pending is combined
  into one message per recipient. After the first message a recipient is told at
  most once every five minutes by later runs — later changes are combined and
  delivered by the next check after that time, and nothing is dropped. Nothing
  starts a check just for that moment: it is the next settings change or the
  hourly check, so such a follow-up message can take up to about an hour. The
  same holds for a report that is completed in the instant after a running
  check has looked at the inbox for the last time. Within
  one run the end-of-run drain may deliver again to a recipient it has just
  told (at most five more times). The record is combined per user
  rather than truncated when it would grow large, so one member's flood can
  never push another member's unreported change out.

- **Nothing leaves the record undelivered.** An announcement is removed only
  after it has reached every channel that is switched on (the command, if one
  is configured, succeeded; the desktop message reached at least one user),
  never by time alone: a long weekend without a login or a command that keeps
  failing does not lose it. After that it is kept for the keep window (three
  days) so that a user who logs in later still gets it, counted from the
  newest change in a combined entry. A command that keeps failing is retried
  without end, with a pause that doubles from five minutes up to six hours
  instead of at every check (decision 0017 has the details).

- **A wrong clock cannot suppress or stall it.** Stored times that lie in the
  future (the clock was ahead and was corrected) are pulled back to now when
  the record and the inbox are read; the rate limit ignores such a stamp, and
  the quiet period is additionally bounded by elapsed time (two minutes), well
  below the unit's start time limit.

- **Delivery is independent of the problem conditions**, so a `settings` change
  is announced even while a backup is in progress. The record is saved after
  each delivery, so a check killed halfway does not resend what it already sent;
  every message is held under a few kilobytes and the hook's stdin write cannot
  block the check, so a huge exclude list can neither make delivery fail nor
  hang it.

### 5. Locking and file permissions

- Every lock file that only root processes need (`.notify.lock`,
  `.notify-settings.lock`, `.current.lock`, and the run lock `run.lock`) is
  created `0600` and tightened to `0600` when it is opened, so a group member
  cannot open it read-only and hold a shared `flock` — which would otherwise
  block the daemon's recording path or keep backups from starting. `nestegg
  status` still works for a group member while a backup runs, because it reads
  `/proc/locks` rather than the lock file. The daemon's wait for the inbox lock
  in the request path is time-bounded, so a stuck lock cannot tie up a worker
  thread forever.
- **Lock files of development versions.** Development versions created these
  files `0644`. There was never a release, tag or package with such files, so
  they exist only on development and test installations. A lock file with
  another mode is tightened to `0600` in place: by every root process that
  opens it, and by `postinst` for all four files (a plain `chmod`, never
  through a symlink, never aborting the upgrade). A lock file is never unlinked
  or replaced, and `/proc/locks` is not consulted for this: replacing a lock
  file can leave two processes each holding a lock on a different inode of "the"
  lock. A descriptor a group member opened before the upgrade keeps working
  until that process ends; on a development machine that is accepted. Every
  wait for one of these locks has a time limit (the run lock half a second,
  `current.json` 30 seconds, the notification check 90 seconds, the inbox
  5 seconds).
- **A run lock that cannot be taken although no backup runs is an error.** A
  backup counts as "already running" (exit code 0, nothing recorded) only when
  `/proc/locks` shows an exclusive lock on the run lock file or a fresh
  `current.json` of another run exists. Otherwise — a stray shared lock, for
  example — the attempt fails with exit code 1 and a message naming the
  processes `/proc/locks` lists as holders, and it is recorded as a failed run
  like a missing key, so the health becomes `failing` and the notification
  check reports it (and the growing pause of decision 0021 applies). A timer
  run that is not due stays quiet as before. The key commands that take the
  run lock give the same message instead of "a backup is running".
- `postinst` only **tightens** a `config.toml` that is too open (readable,
  writable or executable by others, or writable by the group) or wrongly owned;
  it sets the owner to root, and the mode only after the owner change worked. A
  stricter `root:root 0600` an administrator chose is kept; a symlink is never
  followed; and when the `nestegg` group is missing the file falls back to
  root-only `0600` rather than staying world-readable.
- `nestegg init` writes `config.toml` the way the settings writer does: a new
  file created with the final mode `0640` from the start (no window at a looser
  mode, also on `--force`), synced and renamed over any old file, refusing a
  symlink rather than following and truncating it.

## Consequences

- Turning off the protection of this computer's backups is visible: the owner
  (and anyone they told the hook to notify) hears about it, named, within
  seconds, and durably — a crash or a slow hook cannot lose the report.
- The `settings` announcement and the `automatic_off` condition are two more
  message kinds the hook may see (`NESTEGG_EVENT=settings`, `automatic_off`);
  existing hooks ignore unknown events. The detail can contain paths, so it
  follows the two detail levels of [decision 0017](0017-notifications.md):
  members of group `nestegg` and root see it in full, everyone else only learns
  that the settings were changed.
- `tests/e2e/systemd.sh` switches automatic backups off through the API and
  checks that the hook gets the `settings` event with the user's name without
  waiting for the hourly timer, that the `automatic_off` condition follows, that
  an exclude of a whole source is refused with the plain message, and that a
  group member cannot open the daemon's lock files. It also proves the folder
  guard reads as the requester: a member's exclude that re-includes a name in
  another user's `0700` home directory gets the **same** answer (status and
  text) whether that name exists or not, while for the member's own home the
  precise judgement still works (a re-included real entry is accepted, a
  re-included missing one refused).

### What this does and does not guarantee (the limits, plainly)

- **The exclude guard is a guard against accidents and casual misuse, not a
  security boundary.** A group member can still narrow the backup in ways that
  are not refused — excluding a user's folders one by one, for example — but
  every such change is reported, named, to the owner and the hook. The refusal
  only fences off the one-move "drop a whole home, source or system directory"
  cases.
- **Pattern matching is best-effort against an older library.** nestegg uses
  `pathpatterns` 0.1.2; the backup client links 1.0.0 — the same code, an older
  release. The two behave the same on the cases that matter here (confirmed with
  `pxar create --exclude` in the e2e client image), but nestegg does not promise
  byte-for-byte parity; it promises the same syntax. For the malformed
  patterns compared by hand when this was written, both refused the same
  ones (the comparison is not part of the test suite).
- **The requester can write into the answer the guard reads.** The folder is
  listed by a child running as the requester, so the requester can open that
  child's output pipe (`/proc/<pid>/fd/1`) and add entries of their own to the
  list (confirmed by trying it), which can only make the guard more permissive
  for their own request — as a list built around the made-up names in the public
  source can too. The injected data is read under the same bounds (4096 entries,
  a fixed number of bytes) and cannot keep a read open past the deadline, since
  the daemon closes its end and waits only until the deadline, so it cannot hang
  the daemon, crash it, or change what the guard says to another user.
- **root is unbounded in the file.** Every limit above binds only changes made
  through the API by a group member. root can set anything in
  `/etc/nestegg/config.toml`, which is the intended trust boundary.

## Alternatives rejected

- **Only report, do not limit.** Reporting alone leaves the extreme
  combinations possible: a determined or careless group member could still set a
  weeks-long overdue window or drop a whole home directory, and the owner would
  get one message and then silence. The limits keep the worst cases out of
  reach; the report covers everything else.
- **Settings for administrators only (no group editing).** Taking the editable
  settings away from group members would undo the point of
  [decision 0013](0013-editable-settings-in-the-web-ui.md) — everyday changes
  without `sudo`. Most changes are harmless and useful; only the extremes need
  to be fenced off, and the rest is made visible rather than forbidden.

## History

- **First implementation.** Reporting, the `automatic_off` condition, the
  overdue cap and an exclude rule that matched patterns *syntactically*.
- **First review.** The syntactic rule was both too weak and too strong
  (accepting `alice`, `/[a-z]*` while refusing harmless `/.cache`); it was
  replaced by matching with the client's own `pathpatterns` matcher, the
  pattern-length limit moved to bytes, the overdue cap gained the
  `interval + 72` allowance for a weekly rhythm, and the change was recorded in a
  durable inbox before the HTTP answer.
- **Second review (this revision).** The matcher is now applied **archive-
  relative over the whole new list** (so anchored source paths like `/home` are
  no longer refused in normal mode where they match nothing, and a home
  directory is tested in the archive that really contains it, including its real
  top-level entries); malformed patterns that *panic* the matcher are caught and
  refused instead of returning HTTP 500; the inbox is re-drained at the end of a
  run and paced by a quiet period plus a per-recipient rate limit; the record is
  written ahead as an intent and a damaged inbox is reported; all root-only lock
  files are `0600`; `postinst` also tightens a group-writable or wrongly owned
  file and falls back to `0600` without the group; and `nestegg init` writes the
  file atomically with its final mode and refuses a symlink.
- **Third review (2026-11-10).** The reviewers proposed to replace the
  content-dependent folder guard by a purely computed one. The maintainer
  decided to keep reading folder contents and to close the leak instead. The
  folder guard no longer reads entry names as
  root: it reads them with the requester's own identity (uid and primary gid of
  the socket, no supplementary groups) in a child process with a time limit and
  a bound on how many such reads run at once, so its answer can no longer reveal
  a name in another user's `0700` home directory or in `/root` (a group member
  could otherwise tell an existing name from a missing one by whether a
  `!`-re-include change was accepted). The guard became the **last** validation
  step, so a refusal for another reason (for example Wake-on-LAN without a
  `[wake]` section) never reads a folder and so cannot be used to probe without
  a trace. An **excluded ancestor directory** of a protected folder now counts
  as emptying it (a parent such as `/var/home` above `/var/home/alice`), which
  the earlier version missed. The old in-thread read with a timeout, which left
  a thread hanging on a dead file system and added one per further request, is
  replaced by the bounded child-process reader. In the same round:
  announcements are no longer dropped by time alone and a clock that was ahead
  can neither suppress nor stall them (section 4); old `0644` lock files are
  replaced and every lock wait is bounded (section 5).
- **Fourth review (2026-11-10).** All folder reads of one request now share one
  deadline of four seconds (a folder not read by then is unknown), the read no
  longer uses a reader thread and the wait for the child has the same
  deadline; a folder that is a symbolic link is unknown on purpose, as the
  client does not follow links; the wording of "the last step" is made precise
  (the last validation of the request's values); and what a requester can do by
  writing into the child's output is recorded in the limits.
  The replacement of old `0644` lock files was removed again: it was faulty (a
  `/proc/locks` line of a process that was gone stopped it for good, two
  processes could each end up with "the" lock, and the device numbers of
  `stat` and `/proc/locks` need not match on btrfs), and such files exist only
  on development installations. Lock files are now only tightened in place,
  and a blocked run lock without a run is an error instead of a silent
  "already running" (section 5).
  A settings announcement now counts as delivered on the desktop only when a
  member of group `nestegg` got the full text; users who only get the general
  text do not release it. If no member ever logs in, the wait ends 30 days
  after the newest change, with one line in the system log, so the record
  stays small. While the command for a settings announcement keeps failing,
  every check ends with a non-zero exit code, also during the pause between
  attempts, so the unit's failed state does not flip (decision 0017).
