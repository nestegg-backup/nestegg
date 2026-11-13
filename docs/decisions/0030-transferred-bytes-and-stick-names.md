# 0030 – Transferred bytes and stick names

- Status: implemented
- Date: 2026-11-13
- Extends: 0014, 0026, 0029

## Context

The run history labelled `Run.uploaded_bytes` as "Uploaded". That value is the
client's "had to backup X": the new data before compression. A real run printed
`system.pxar: had to backup 1.925 GiB of 1.948 GiB (compressed 744.728 MiB)`
and the page claimed about 1.9 GiB were uploaded, while about 745 MiB went over
the network.

With several registered sticks the restore source choice listed bare UUIDs;
the person at the computer knows the sticks as "the blue one" or "the office
one", not by their partition UUID.

## Decision

**Transferred bytes.** `Run.transferred_bytes` is added: the sum of the
client's "(compressed X)" per archive, i.e. what was sent to the server or
written to the stick. It is updated after each finished archive, like
`uploaded_bytes`. `uploaded_bytes` keeps its value and is now documented as
"new data before compression"; its name stays for backward compatibility of
the API and of stored run files. A client line without "(compressed …)" still
parses; that archive makes the transferred sum unknown (null) rather than a
partial sum that would look exact. The snapshot report for other computers
carries the new field as well.

The web UI and `nestegg history`/`status` show only `transferred_bytes`, as
"uploaded" (server) or "written" (stick). Runs recorded by older versions have
no such value and show no amount: showing the uncompressed figure under the
same label would repeat the overstatement, and labelling a second figure per
row would clutter the table. The live step text during a run uses the same
field.

**Stick names.** `[stick.names]` maps a registered, lower-case data UUID to
an optional name: 1 to 40 characters, no leading or trailing white space, no
control characters and no invisible formatting characters (zero-width,
bidirectional overrides, soft hyphen), so two names cannot be made to look the
same. A name for an unregistered UUID is an error, as are unknown keys
anywhere in the file; configurations without names stay valid. The name is
set with `nestegg stick add --name "<text>"` (checked before the stick is
written) and changed or removed later with `nestegg stick name --uuid <UUID>
"<text>"` / `--clear`; both edit the file in place and keep comments.

The API adds the name where stick UUIDs are listed:
`TargetStatus.sticks[uuid].name` and `ConfigView.stick_names`. Requests
still select a stick by UUID only. The UI shows the name with the UUID as
secondary detail (restore source choice, the stick row of "Backup
destinations" when there are several sticks or a name, the key reminder);
without a name it shows "Stick" plus the first eight UUID characters, more
when another registered stick starts the same way, because the daemon does
not know a size or model per medium without probing it. The CLI shows
"name (UUID)" or "stick UUID".

## Consequences

- API contract 0.6.0, additive only; older clients ignore the new fields and
  older daemons simply lack them.
- Stored run files of older versions read as before; the new field is
  omitted from files while it is unknown.
- A stick name is display data in a root-owned file; it never selects a
  device or appears in a command line the UI offers.
