# Working rules for this repository

This repository is private today and will be published as open source later.
Everything committed here must be safe to publish. These rules apply to humans
and coding agents alike.

## Never commit

- Real IP addresses, host names, domains, MAC addresses or network layouts of
  any real installation (also no Tailscale/VPN names).
- Credentials of any kind: passwords, PBS API tokens and their secrets,
  encryption keys or passphrases, certificate fingerprints of real servers,
  SSH keys.
- Personal data: real names, user names, e-mail addresses, machine names,
  home directory paths of a real user, screenshots or logs from a real system.
- Copies of private infrastructure notes. Information from a maintainer's own
  homelab may inform requirements, but it is written down here only in generic
  form ("the PBS may be asleep and needs Wake-on-LAN"), never with real values.

## Use these placeholders instead

| Kind | Placeholder |
|---|---|
| IPv4 | `192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24` (RFC 5737) |
| IPv6 | `2001:db8::/32` (RFC 3849) |
| Host / domain | `pbs.example.com`, `desktop.example.com` (RFC 2606) |
| PBS repository | `backup@pbs!desktop@pbs.example.com:8007:datastore` |
| Fingerprint, secret | `<FINGERPRINT>`, `<TOKEN-SECRET>` or an environment variable |
| User / home | `alice`, `/home/alice` |
| MAC address | `00:00:5e:00:53:01` (RFC 7042 documentation range) |

## Safety net

- `scripts/privacy-check.sh` runs as `pre-commit` and `commit-msg` hook
  (`git config core.hooksPath .githooks`). It requires a GitHub noreply commit
  e-mail, runs gitleaks with `.gitleaks.toml`, and checks a local deny list in
  `.git/info/privacy-denylist` (never committed - each maintainer keeps their
  own personal strings there).
- CI runs gitleaks on every push and pull request.
- Before the repository is made public: run `scripts/privacy-check.sh tree`,
  scan the full history with `gitleaks detect`, and review every commit author
  and message.

## Project conventions

- Several agents may work on this project at the same time. Each agent works
  in its own git worktree and branch and only touches the paths it owns
  (see the repository layout in `docs/architecture.md`). Before committing,
  check `git status` for files you did not create.
- Repository documentation and code comments are written in English.
- Decisions are recorded in `docs/decisions/` (one file per decision).
- Do not reimplement the PBS protocol, chunk store or encryption; build on
  `proxmox-backup-client` or the official Proxmox Rust crates.
