//! Guards for the sandboxing of the shipped systemd units.
//!
//! The backup unit reads the whole file system (`/` as `system.pxar`) and
//! the daemon unit runs API restores into home directories, `/tmp` and
//! root-owned directories. Sandbox options that hide, replace or lock paths
//! do not make either fail loudly: the backup silently archives an empty
//! placeholder (e.g. `ProtectKernelModules=yes` mounts an empty tmpfs over
//! `/usr/lib/modules`), and restores fail with "Read-only file system" or
//! land in a private `/tmp`. See docs/decisions/0007-systemd-unit-sandboxing.md.

use std::path::Path;

const OFF: &[&str] = &["no", "false", "off", "0"];

/// Options that hide, replace, remount or lock paths in the unit's mount
/// namespace, forbid setuid/setgid bits, or change the identity files are
/// read and written with. Each is allowed only with the listed values.
const FORBIDDEN: &[(&str, &[&str])] = &[
    ("PrivateTmp", OFF),
    ("ProtectSystem", OFF),
    ("ProtectHome", OFF),
    ("ProtectKernelModules", OFF),
    ("PrivateDevices", OFF),
    ("PrivateUsers", OFF),
    ("DynamicUser", OFF),
    ("RestrictSUIDSGID", OFF),
    ("ProtectProc", &["default"]),
    ("ProcSubset", &["all"]),
    ("User", &["root", "0"]),
    ("Group", &["root", "0"]),
    ("InaccessiblePaths", &[]),
    ("InaccessibleDirectories", &[]),
    ("ReadOnlyPaths", &[]),
    ("ReadOnlyDirectories", &[]),
    ("TemporaryFileSystem", &[]),
    ("BindPaths", &[]),
    ("BindReadOnlyPaths", &[]),
    ("RootDirectory", &[]),
    ("RootImage", &[]),
    ("MountImages", &[]),
    ("ExtensionImages", &[]),
    ("ExtensionDirectories", &[]),
];

/// Capabilities that backups (reading every file, xattrs, ACLs) and
/// restores (owners, modes incl. setuid/setgid, device nodes, file
/// capabilities, immutable flags, dropping to the requesting user) need.
const REQUIRED_CAPS: &[&str] = &[
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_MKNOD",
    "CAP_SETUID",
    "CAP_SETGID",
    "CAP_SETFCAP",
    "CAP_LINUX_IMMUTABLE",
    "CAP_SYS_ADMIN",
];

/// System call groups that may appear in a `SystemCallFilter=~...` deny list.
const DENYABLE_SYSCALL_GROUPS: &[&str] = &[
    "@module",
    "@reboot",
    "@swap",
    "@obsolete",
    "@cpu-emulation",
    "@debug",
    "@raw-io",
    "@clock",
];

/// `key=value` lines of the `[Service]` section, in file order.
fn service_settings(text: &str) -> Vec<(String, String)> {
    let mut section = "";
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line;
            continue;
        }
        if section == "[Service]"
            && let Some((key, value)) = line.split_once('=')
        {
            out.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    out
}

/// Settings of a unit text that conflict with full-system backups or
/// restores, as human-readable messages.
fn violations(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if text.lines().any(|l| l.trim_end().ends_with('\\')) {
        out.push("line continuations are not supported by this check".into());
    }
    let settings = service_settings(text);
    if settings.is_empty() {
        out.push("no [Service] settings found".into());
    }
    for (key, value) in &settings {
        if let Some((_, allowed)) = FORBIDDEN.iter().find(|(k, _)| k == key)
            && !allowed.contains(&value.to_ascii_lowercase().as_str())
        {
            out.push(format!(
                "{key}={value} hides or locks paths (or changes identities) that \
                 full-system backups and restores need"
            ));
        }
        if key == "CapabilityBoundingSet" && !value.is_empty() {
            if !value.starts_with('~') {
                out.push(format!(
                    "CapabilityBoundingSet must be a deny list (~...), got {value}"
                ));
            }
            for cap in value.trim_start_matches('~').split_whitespace() {
                if REQUIRED_CAPS.contains(&cap.to_ascii_uppercase().as_str()) {
                    out.push(format!("CapabilityBoundingSet must not drop {cap}"));
                }
            }
        }
        if key == "SystemCallFilter" && !value.is_empty() {
            if !value.starts_with('~') {
                out.push(format!(
                    "SystemCallFilter must be a deny list (~...), got {value}"
                ));
            }
            for group in value.trim_start_matches('~').split_whitespace() {
                if !DENYABLE_SYSCALL_GROUPS.contains(&group) {
                    out.push(format!("SystemCallFilter must not deny {group}"));
                }
            }
        }
    }
    // Module loading stays blocked, without hiding /usr/lib/modules.
    for (key, value) in [
        ("CapabilityBoundingSet", "~CAP_SYS_MODULE"),
        ("SystemCallFilter", "~@module"),
    ] {
        if !settings.iter().any(|(k, v)| k == key && v == value) {
            out.push(format!("expected {key}={value}"));
        }
    }
    out
}

fn check_unit(unit: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packaging/systemd")
        .join(unit);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let found = violations(&text);
    assert!(
        found.is_empty(),
        "{unit} (see the comment in the unit and decision 0007):\n  {}",
        found.join("\n  ")
    );
}

#[test]
fn backup_unit_sees_the_whole_file_system() {
    check_unit("nestegg-backup.service");
}

#[test]
fn daemon_unit_allows_every_restore_target() {
    check_unit("nestegg.service");
}

#[test]
fn the_check_rejects_the_sandboxing_that_broke_backups_and_restores() {
    let old_backup_unit = "[Service]\nNoNewPrivileges=yes\nPrivateTmp=yes\n\
        ProtectKernelTunables=yes\nProtectKernelModules=yes\nProtectControlGroups=yes\n";
    let found = violations(old_backup_unit).join("\n");
    assert!(found.contains("PrivateTmp=yes"), "{found}");
    assert!(found.contains("ProtectKernelModules=yes"), "{found}");
    let old_daemon_unit = "[Service]\nPrivateTmp=yes\nProtectSystem=full\n\
        ProtectHome=read-only\nRestrictSUIDSGID=yes\n\
        CapabilityBoundingSet=~CAP_SYS_MODULE CAP_CHOWN\nSystemCallFilter=~@module @privileged\n";
    let found = violations(old_daemon_unit).join("\n");
    for bad in [
        "ProtectSystem=full",
        "ProtectHome=read-only",
        "RestrictSUIDSGID=yes",
        "must not drop CAP_CHOWN",
        "must not deny @privileged",
    ] {
        assert!(found.contains(bad), "{bad} not reported in:\n{found}");
    }
    // Settings outside [Service] and explicit "off" values are fine.
    let ok = "[Unit]\nPrivateTmp=yes\n[Service]\nProtectHome=no\n\
        CapabilityBoundingSet=~CAP_SYS_MODULE\nSystemCallFilter=~@module\n";
    assert!(violations(ok).is_empty(), "{:?}", violations(ok));
}

/// Manual runs are started by the daemon as transient units
/// (`systemd-run --property=...`); their properties follow the same rules.
#[test]
fn manual_run_unit_properties_hide_nothing() {
    let source = include_str!("../src/daemon.rs");
    let keys: Vec<&str> = source
        .split("--property=")
        .skip(1)
        .map(|part| part.split('=').next().unwrap_or(""))
        .collect();
    assert!(
        !keys.is_empty(),
        "no systemd-run properties found in daemon.rs"
    );
    for key in keys {
        assert!(
            !FORBIDDEN.iter().any(|(k, _)| *k == key),
            "daemon.rs starts manual runs with --property={key}, which hides or locks \
             paths that a full-system backup needs"
        );
    }
}
