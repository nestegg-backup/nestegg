# Security policy

nestegg has not been released yet (see the status in the [README](README.md)).
There are no supported versions; security fixes go into `main`.

## Reporting a vulnerability

Please do not open a public issue, pull request or discussion for a
security problem. Report it privately through GitHub instead: open the
**Security** tab of this repository and choose **Report a vulnerability**.

Helpful in a report:

- what is affected: a command, the local web interface, the hub, the rescue
  stick or the backup stick
- the commit or version, the distribution and how nestegg was installed
- the steps that show the problem, and what an attacker gains

Do not include real credentials, encryption keys, paper keys, server
addresses or files from a real backup. Use placeholders such as
`pbs.example.com` and `<TOKEN-SECRET>`.

## What to expect

nestegg is maintained in spare time, so there is no guaranteed response
time. Every report gets an answer, and a fix is agreed with the reporter
before details are made public.

## Scope

Especially relevant:

- anything that exposes the encryption key, the paper key or the PBS
  credentials, or writes them into logs, status files or the web interface
- a local user or a web page reaching the local web interface and starting
  backups, restores or setting changes they should not be allowed to
- a backup that is shown as complete or verified although it is not
- the rescue or backup stick writing to, erasing or restoring onto the wrong
  disk

Problems in `proxmox-backup-client` or the Proxmox Backup Server itself are
out of scope here; please report those to the Proxmox project.
