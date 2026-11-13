//! Conservative repair of hardware-specific network pins in an offline tree.
//! Only literal, structurally identified wired selectors may be rewritten.
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug)]
struct Pin {
    start: usize,
    mac: String,
    wired: bool,
    wireless: bool,
}

fn mac(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    (bytes.len() == 17
        && bytes.iter().enumerate().all(|(i, c)| {
            if i % 3 == 2 {
                *c == b':'
            } else {
                c.is_ascii_hexdigit()
            }
        }))
    .then(|| text.to_ascii_lowercase())
}

/// Restrict automatic handling to single-line literal block mappings. In
/// other YAML, text that resembles a selector might itself be a Wi-Fi secret.
fn literal_yaml(text: &str) -> bool {
    !text.lines().any(|l| {
        let l = l.split('#').next().unwrap_or("");
        l.contains(['{', '}', '[', ']', '&', '*', '!', '|', '>', '\t'])
            || l.trim() == "---"
            || l.trim() == "..."
            // Multi-line quoted scalars can contain text resembling mapping
            // keys. Never interpret their contents as configuration.
            || l.matches('\'').count() % 2 != 0
            || l.matches('"').count() % 2 != 0
            || l.contains('\\')
    })
}

/// Scan selectors, never connection passwords or other values. Complex YAML
/// is detection-only: the block parser deliberately does not interpret it.
fn pins(text: &str, kind: &str) -> Vec<Pin> {
    let mut out = Vec::new();
    let mut section = "";
    let mut stack: Vec<(usize, &str)> = Vec::new();
    let simple_yaml = literal_yaml(text);
    let nm_wired = matches!(
        ini_value(text, "[connection]", "type"),
        Some("ethernet" | "802-3-ethernet")
    ) && !text
        .lines()
        .any(|l| matches!(l.trim(), "[wifi]" | "[802-11-wireless]"));
    let nm_wireless = matches!(
        ini_value(text, "[connection]", "type"),
        Some("wifi" | "802-11-wireless")
    );
    let link_wired = ini_value(text, "[Match]", "Type") == Some("ether");
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let body = line.split('#').next().unwrap_or("").trim();
        if body.starts_with('[') {
            section = body;
        }
        let indent = line.len() - line.trim_start().len();
        let key_value = body.split_once(':');
        let assignment = body
            .split_once('=')
            .map(|(key, value)| (key.trim(), value.trim()));
        if kind == "netplan" && !body.is_empty() {
            while stack.last().is_some_and(|(i, _)| *i >= indent) {
                stack.pop();
            }
        }
        let yaml_pin = kind == "netplan"
            && ["macaddress:", "\"macaddress\":", "'macaddress':"]
                .iter()
                .any(|key| body.contains(key));
        let selected = yaml_pin
            || (kind == "nm" && assignment.is_some_and(|(key, _)| key == "mac-address"))
            || (kind == "link"
                && section == "[Match]"
                && assignment
                    .is_some_and(|(key, _)| matches!(key, "MACAddress" | "PermanentMACAddress")))
            || (kind == "udev"
                && (body.contains("ATTR{address}") || body.contains("ATTRS{address}")))
            || (kind == "ifupdown" && body.split_whitespace().next() == Some("hwaddress"));
        if selected {
            let wired = if yaml_pin {
                simple_yaml
                    && key_value.is_some_and(|(k, v)| {
                        k == "macaddress" && mac(v.trim().trim_matches(['\"', '\''])).is_some()
                    })
                    && matches!(
                        stack.as_slice(),
                        [(_, "network"), (_, "ethernets"), (_, _), (_, "match")]
                    )
            } else {
                let literal = assignment.and_then(|(_, v)| mac(v)).is_some();
                literal
                    && ((kind == "nm"
                        && nm_wired
                        && matches!(section, "[ethernet]" | "[802-3-ethernet]"))
                        || (kind == "link" && link_wired))
            };
            let wireless = (yaml_pin
                && matches!(
                    stack.as_slice(),
                    [(_, "network"), (_, "wifis"), (_, _), (_, "match")]
                ))
                || (kind == "nm"
                    && nm_wireless
                    && matches!(section, "[wifi]" | "[802-11-wireless]"));
            // The only replacements are these exact 17 ASCII bytes, not a
            // global replacement that could alter a Wi-Fi password.
            let selector = if yaml_pin {
                body.find("macaddress").map(|i| &body[i..]).unwrap_or(body)
            } else {
                body
            };
            let selector_start = line.find(selector).unwrap_or(0);
            for (start, _) in selector.char_indices() {
                if let Some(value) = selector.get(start..start + 17).and_then(mac) {
                    out.push(Pin {
                        start: offset + selector_start + start,
                        mac: value,
                        wired,
                        wireless,
                    });
                }
            }
        }
        if kind == "netplan"
            && let Some((key, value)) = key_value
            && value.trim().is_empty()
        {
            stack.push((indent, key));
        }
        offset += line.len();
    }
    out
}

/// Duplicate INI keys/sections are detection-only rather than guessing which
/// value a renderer will use.
fn ini_value<'a>(text: &'a str, wanted: &str, key: &str) -> Option<&'a str> {
    let mut section = "";
    let mut values = Vec::new();
    let mut sections = 0;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
            if section == wanted {
                sections += 1;
            }
        }
        if section == wanted
            && let Some((k, value)) = line.split_once('=')
            && k.trim() == key
        {
            values.push(value.trim());
        }
    }
    if sections == 1 && values.len() == 1 {
        Some(values[0])
    } else {
        None
    }
}

/// The first component of `rel` below `root` that is a symbolic link, if any.
/// Such a directory would be read (and written) wherever the link points in
/// the rescue system, not inside the restored tree.
fn linked_component(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for part in Path::new(rel).components() {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => return Some(path),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

/// A configuration file and its kind ("netplan", "nm", ...).
type ConfigFile = (PathBuf, &'static str);

/// The configuration files to look at, and the places that were skipped
/// because the way to them leads through a symbolic link.
fn files(root: &Path) -> Result<(Vec<ConfigFile>, Vec<PathBuf>)> {
    let mut files = Vec::new();
    let mut linked = Vec::new();
    for (dir, extension, kind) in [
        ("etc/netplan", "yaml", "netplan"),
        ("etc/NetworkManager/system-connections", "", "nm"),
        ("etc/systemd/network", "link", "link"),
        ("etc/network/interfaces.d", "", "ifupdown"),
    ] {
        if let Some(link) = linked_component(root, dir) {
            linked.push(link);
            continue;
        }
        match fs::read_dir(root.join(dir)) {
            Ok(entries) => {
                for e in entries {
                    let p = e?.path();
                    if !p
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .starts_with('.')
                        && !p.to_string_lossy().ends_with(".bak")
                        && (extension.is_empty() || p.extension().is_some_and(|e| e == extension))
                    {
                        files.push((p, kind));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    for (p, kind) in [
        ("etc/udev/rules.d/70-persistent-net.rules", "udev"),
        ("etc/network/interfaces", "ifupdown"),
    ] {
        // The parent directories; a link at the file itself is handled by
        // the caller like any other linked file.
        let parent = Path::new(p).parent().and_then(Path::to_str).unwrap_or("");
        if let Some(link) = linked_component(root, parent) {
            linked.push(link);
            continue;
        }
        if fs::symlink_metadata(root.join(p)).is_ok() {
            files.push((root.join(p), kind));
        }
    }
    files.sort();
    linked.sort();
    linked.dedup();
    Ok((files, linked))
}

/// Discover physical wired NICs in the *rescue* system. Virtual interfaces
/// have no device link; Wi-Fi interfaces expose wireless/phy80211 in sysfs.
fn hardware(sys: &Path) -> Result<(BTreeSet<String>, Vec<String>)> {
    let mut present = BTreeSet::new();
    let mut wired = Vec::new();
    for entry in fs::read_dir(sys)? {
        let p = entry?.path();
        let value = fs::read_to_string(p.join("address"))?;
        if let Some(mac) = mac(value.trim()) {
            present.insert(mac.clone());
            if p.join("device").exists()
                && !p.join("wireless").exists()
                && !p.join("phy80211").exists()
                && fs::read_to_string(p.join("type"))?.trim() == "1"
            {
                wired.push(mac);
            }
        }
    }
    Ok((present, wired))
}

pub fn fix_network(root: &Path, sys: &Path) -> Result<Vec<String>> {
    let root = root.canonicalize()?;
    let mut notes = Vec::new();
    let manual = "Manual fix: compare 'ip -br link' on the restored system with the MAC match and interface name in this file; update the MAC selector (and any set-name/name reference) for the intended NIC, then apply the network configuration or reboot.";
    let (present, wired, hardware_known) = match hardware(sys) {
        Ok((present, wired)) => (present, wired, true),
        Err(_) => {
            notes.push("Network: hardware enumeration failed; no configuration changed. Check MAC selectors manually using ip -br link.".into());
            (BTreeSet::new(), Vec::new(), false)
        }
    };
    let mut configs = Vec::new();
    let mut safe = hardware_known;
    let (found, linked) = files(&root)?;
    for link in linked {
        notes.push(format!(
            "Network: {} is a symbolic link, so the configuration below it was not read; unchanged. {manual}",
            link.strip_prefix(&root)?.display()
        ));
        safe = false;
    }
    for (path, kind) in found {
        // A link is never followed: its target may only exist on the running
        // system (`/run`), or resolve differently here than after the first
        // boot. A link to /dev/null is the usual way to mask a file.
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if fs::read_link(&path).is_ok_and(|to| to == Path::new("/dev/null")) {
                    continue;
                }
                notes.push(format!(
                    "Network: {} is a symbolic link; unchanged. {manual}",
                    path.strip_prefix(&root)?.display()
                ));
                safe = false;
                continue;
            }
            Ok(_) => {}
            Err(_) => {
                notes.push(format!(
                    "Network: {} could not be inspected; unchanged. {manual}",
                    path.strip_prefix(&root)?.display()
                ));
                safe = false;
                continue;
            }
        }
        if !path.is_file() {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => {
                safe = false;
                notes.push(format!(
                    "Network: {} could not be read; unchanged. {manual}",
                    path.strip_prefix(&root)?.display()
                ));
                continue;
            }
        };
        if kind == "netplan" && !literal_yaml(&text) {
            safe = false;
            notes.push(format!("Network: /{}: complex YAML may contain MAC selectors; unchanged (not interpreted, to protect Wi-Fi secrets). {manual} Review cloud-init datasource/network-config too: it may regenerate this file at boot.", path.strip_prefix(&root)?.display()));
            continue;
        }
        let pins = pins(&text, kind);
        configs.push((path, text, pins));
    }
    let old: BTreeSet<_> = configs
        .iter()
        .flat_map(|(_, _, pins)| pins)
        .map(|p| p.mac.clone())
        .collect();
    let missing: BTreeSet<_> = old.difference(&present).cloned().collect();
    let wired_old: BTreeSet<_> = configs
        .iter()
        .flat_map(|(_, _, pins)| pins)
        .filter(|p| p.wired)
        .map(|p| p.mac.clone())
        .collect();
    let rewrite = safe
        && wired_old.len() == 1
        && wired_old.difference(&present).count() == 1
        && wired.len() == 1
        && configs
            .iter()
            .flat_map(|(_, _, pins)| pins)
            .all(|p| p.wired || p.wireless);
    for (path, text, pins) in configs {
        if pins.is_empty() {
            continue;
        }
        let name = format!("/{}", path.strip_prefix(&root)?.display());
        let mut new = text.clone();
        for pin in pins.iter().rev() {
            if pin.wired && missing.contains(&pin.mac) && rewrite {
                new.replace_range(pin.start..pin.start + 17, &wired[0]);
            }
        }
        if new != text {
            // Hidden, non-config suffix: none of the network renderers load
            // this copy. Never overwrite an earlier rescue's backup.
            let mut backup = path.with_file_name(format!(
                ".{}.nestegg-rescue.bak",
                path.file_name().unwrap().to_string_lossy()
            ));
            let mut saved = loop {
                match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&backup)
                {
                    Ok(file) => break file,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        backup = path.with_file_name(format!(
                            ".{}.nestegg-rescue-{:016x}.bak",
                            path.file_name().unwrap().to_string_lossy(),
                            rand::random::<u64>()
                        ));
                    }
                    Err(e) => return Err(e).with_context(|| format!("cannot back up {name}")),
                }
            };
            use std::io::Write;
            fs::set_permissions(&backup, fs::metadata(&path)?.permissions())?;
            saved.write_all(text.as_bytes())?;
            saved.sync_all()?;
            fs::write(&path, new)?;
            notes.push(format!("Network: {name}: rewrote absent wired MAC {} to {}; backup next to file: {}. {manual}", wired_old.first().unwrap(), wired[0], backup.file_name().unwrap().to_string_lossy()));
        } else {
            for pin in pins {
                notes.push(format!("Network: {name}: MAC {} {}; unchanged ({} physical wired NICs, mapping may be ambiguous or unsupported). {manual}", pin.mac, if !hardware_known { "presence unknown" } else if missing.contains(&pin.mac) { "absent on this machine" } else { "present on this machine" }, wired.len()));
            }
        }
        if name.contains("netplan") || root.join("etc/cloud").exists() {
            notes.push("Network: cloud-init may regenerate network configuration at boot. Update its datasource/network-config too, or deliberately disable cloud-init network rendering (network: {config: disabled}) after checking the generated configuration. Rescue does not change cloud-init policy or Wi-Fi secrets.".into());
        }
    }
    if old.is_empty() {
        notes.push("Network: no supported MAC-pinned selectors found; configuration unchanged. If networking fails, check interface names and MAC selectors using ip -br link.".into());
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    const OLD: &str = "00:00:5e:00:53:01";
    fn new_mac() -> String {
        format!("{}2", &OLD[..16])
    }
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("target");
        let sys = d.path().join("sys");
        fs::create_dir_all(&root).unwrap();
        nic(&sys, "enp1s0", &new_mac(), false);
        (d, root, sys)
    }
    fn nic(sys: &Path, name: &str, address: &str, wifi: bool) {
        let p = sys.join(name);
        fs::create_dir_all(p.join("device")).unwrap();
        fs::write(p.join("address"), address).unwrap();
        fs::write(p.join("type"), "1\n").unwrap();
        if wifi {
            fs::create_dir(p.join("wireless")).unwrap();
        }
    }
    fn config(root: &Path, path: &str, text: &str) -> PathBuf {
        let p = root.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        p
    }
    fn netplan() -> String {
        format!(
            "# generated by cloud-init\nnetwork:\n  version: 2\n  ethernets:\n    enp1s0:\n      match:\n        macaddress: '{OLD}'\n      set-name: enp1s0\n      dhcp4: true\n"
        )
    }
    #[test]
    fn netplan_rewrites_only_the_selector_and_keeps_a_private_adjacent_backup() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, root, sys) = fixture();
        let text = netplan();
        let p = config(&root, "etc/netplan/50-cloud-init.yaml", &text);
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        let notes = fix_network(&root, &sys).unwrap().join("\n");
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            text.replace(OLD, &new_mac())
        );
        let bak = p.with_file_name(".50-cloud-init.yaml.nestegg-rescue.bak");
        assert_eq!(fs::read_to_string(&bak).unwrap(), text);
        assert_eq!(
            fs::metadata(bak).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(notes.contains("rewrote absent wired MAC"));
        assert!(notes.contains("Manual fix:") && notes.contains("cloud-init may regenerate"));
        let notes = fix_network(&root, &sys).unwrap().join("\n");
        assert!(notes.contains("present on this machine"));
    }
    #[test]
    fn links_are_never_followed_and_never_stop_the_restore() {
        let (_d, root, sys) = fixture();
        let text = netplan();
        let p = config(&root, "etc/netplan/50-cloud-init.yaml", &text);
        // A link into /run (not part of a backup) and one whose target only
        // exists on the running system: both dangle in the rescue system.
        let nm = root.join("etc/NetworkManager/system-connections");
        fs::create_dir_all(&nm).unwrap();
        std::os::unix::fs::symlink(
            "/run/generated/wired.nmconnection",
            nm.join("wired.nmconnection"),
        )
        .unwrap();
        let notes = fix_network(&root, &sys).unwrap().join("\n");
        assert!(
            notes.contains("wired.nmconnection is a symbolic link; unchanged"),
            "{notes}"
        );
        // An unreadable part of the picture: nothing is rewritten.
        assert_eq!(fs::read_to_string(&p).unwrap(), text);
    }
    #[test]
    fn a_linked_configuration_directory_is_not_read_or_written() {
        let (d, root, sys) = fixture();
        // Stands for a path of the rescue system the link points to.
        let outside = d.path().join("outside-netplan");
        let text = netplan();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("50-cloud-init.yaml"), &text).unwrap();
        fs::create_dir_all(root.join("etc")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("etc/netplan")).unwrap();
        let notes = fix_network(&root, &sys).unwrap().join("\n");
        assert!(notes.contains("etc/netplan is a symbolic link"), "{notes}");
        assert_eq!(
            fs::read_to_string(outside.join("50-cloud-init.yaml")).unwrap(),
            text
        );
        assert_eq!(
            fs::read_dir(&outside).unwrap().count(),
            1,
            "no backup copy either"
        );
    }
    #[test]
    fn a_file_masked_with_dev_null_is_ignored() {
        let (_d, root, sys) = fixture();
        let text = netplan();
        let p = config(&root, "etc/netplan/50-cloud-init.yaml", &text);
        let links = root.join("etc/systemd/network");
        fs::create_dir_all(&links).unwrap();
        std::os::unix::fs::symlink("/dev/null", links.join("99-default.link")).unwrap();
        let notes = fix_network(&root, &sys).unwrap().join("\n");
        assert!(!notes.contains("99-default.link"), "{notes}");
        // The mask does not make the single wired pin ambiguous.
        assert!(
            fs::read_to_string(&p).unwrap().contains(&new_mac()),
            "{notes}"
        );
    }
    #[test]
    fn nm_and_link_selectors_share_one_mapping() {
        let (_d, root, sys) = fixture();
        let configs = [
            (
                "etc/NetworkManager/system-connections/wired.nmconnection",
                format!("[connection]\ntype = ethernet\n[ethernet]\nmac-address = {OLD}\n"),
            ),
            (
                "etc/systemd/network/10-wired.link",
                format!("[Match]\nType = ether\nMACAddress = {OLD}\n[Link]\nName=lan0\n"),
            ),
        ];
        for (path, text) in &configs {
            config(&root, path, text);
        }
        fix_network(&root, &sys).unwrap();
        for (path, text) in configs {
            assert_eq!(
                fs::read_to_string(root.join(path)).unwrap(),
                text.replace(OLD, &new_mac())
            );
        }
    }
    #[test]
    fn ambiguous_hardware_or_multiple_old_macs_are_not_changed() {
        for extra_nic in [false, true] {
            let (_d, root, sys) = fixture();
            let mut text = netplan();
            if extra_nic {
                let other_mac = format!("{}3", &OLD[..16]);
                nic(&sys, "enp2s0", &other_mac, false);
            } else {
                text.push_str(&format!(
                    "    second:\n      match:\n        macaddress: {}\n",
                    new_mac()
                ));
            }
            let p = config(&root, "etc/netplan/50-test.yaml", &text);
            let notes = fix_network(&root, &sys).unwrap().join("\n");
            assert_eq!(fs::read_to_string(p).unwrap(), text);
            assert!(notes.contains("unchanged") && notes.contains("Manual fix:"));
        }
    }
    #[test]
    fn wifi_and_uncertain_formats_are_reported_without_changes_or_secrets() {
        for (path, text) in [
            (
                "etc/netplan/50-test.yaml",
                netplan().replace("ethernets:", "wifis:"),
            ),
            (
                "etc/netplan/50-test.yaml",
                netplan().replace("network:", "network: &alias"),
            ),
            (
                "etc/NetworkManager/system-connections/wifi",
                format!(
                    "[connection]\ntype=wifi\n[wifi]\nmac-address={OLD}\n[wifi-security]\npsk=private-test-password\n"
                ),
            ),
            (
                "etc/systemd/network/10-test.link",
                format!("[Match]\nMACAddress={OLD}\n"),
            ),
            (
                "etc/systemd/network/10-test.link",
                format!("[Match]\nType=ether\nMACAddress=!{OLD}\n"),
            ),
            (
                "etc/udev/rules.d/70-persistent-net.rules",
                format!("SUBSYSTEM==\"net\", ATTR{{address}}==\"{OLD}\", NAME=\"eth0\"\n"),
            ),
            (
                "etc/network/interfaces",
                format!("iface eth0 inet dhcp\n\thwaddress\tether\t{OLD}\n"),
            ),
        ] {
            let (_d, root, sys) = fixture();
            let p = config(&root, path, &text);
            let notes = fix_network(&root, &sys).unwrap().join("\n");
            assert_eq!(fs::read_to_string(p).unwrap(), text);
            assert!(
                notes.contains("absent on this machine") || notes.contains("complex YAML"),
                "{notes}"
            );
            assert!(!notes.contains("private-test-password"));
        }
    }
    #[test]
    fn wifi_hardware_is_not_a_wired_target_and_existing_macs_are_unchanged() {
        for wifi in [false, true] {
            let (_d, root, sys) = fixture();
            fs::remove_dir_all(&sys).unwrap();
            let new = new_mac();
            nic(&sys, "nic0", if wifi { &new } else { OLD }, wifi);
            let text = netplan();
            let p = config(&root, "etc/netplan/50-test.yaml", &text);
            let notes = fix_network(&root, &sys).unwrap().join("\n");
            assert_eq!(fs::read_to_string(p).unwrap(), text);
            assert!(notes.contains("unchanged"));
        }
    }
    #[test]
    fn matching_mac_in_a_wifi_password_is_never_replaced() {
        let (_d, root, sys) = fixture();
        let text = netplan()
            + &format!(
                "  wifis:\n    wlan0:\n      access-points:\n        example:\n          password: '{OLD}'\n"
            );
        let p = config(&root, "etc/netplan/50-test.yaml", &text);
        let notes = fix_network(&root, &sys).unwrap().join("\n");
        let changed = fs::read_to_string(p).unwrap();
        assert!(changed.contains(&format!("macaddress: '{}'", new_mac())));
        assert!(changed.contains(&format!("password: '{OLD}'")));
        assert!(notes.contains("rewrote absent wired MAC"));
    }
    #[test]
    fn multiline_wifi_secret_cannot_be_mistaken_for_a_selector() {
        let (_d, root, sys) = fixture();
        let text = format!(
            "network:\n  wifis:\n    wlan0:\n      password: '\nnetwork:\n  ethernets:\n    fake:\n      match:\n        macaddress: {OLD}\n'\n"
        );
        let p = config(&root, "etc/netplan/50-test.yaml", &text);
        let notes = fix_network(&root, &sys).unwrap().join("\n");
        assert_eq!(fs::read_to_string(p).unwrap(), text);
        assert!(notes.contains("complex YAML"));
        assert!(!notes.contains(OLD));
    }
    #[test]
    fn a_previous_backup_is_never_overwritten() {
        let (_d, root, sys) = fixture();
        let text = netplan();
        let p = config(&root, "etc/netplan/50-test.yaml", &text);
        let bak = config(
            &root,
            "etc/netplan/.50-test.yaml.nestegg-rescue.bak",
            "earlier backup",
        );
        fix_network(&root, &sys).unwrap();
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            text.replace(OLD, &new_mac())
        );
        assert_eq!(fs::read_to_string(&bak).unwrap(), "earlier backup");
        let extra = fs::read_dir(p.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|f| f != &p && f != &bak)
            .unwrap();
        assert_eq!(fs::read_to_string(extra).unwrap(), text);
    }
    #[test]
    fn a_known_wifi_selector_does_not_make_the_wired_mapping_ambiguous() {
        let (_d, root, sys) = fixture();
        let wifi_mac = format!("{}3", &OLD[..16]);
        nic(&sys, "wlan0", &wifi_mac, true);
        let text = netplan();
        let wired = config(&root, "etc/netplan/50-wired.yaml", &text);
        let wifi_text = format!(
            "[connection]\ntype=wifi\n[wifi]\nmac-address={wifi_mac}\n[wifi-security]\npsk=private-test-password\n"
        );
        let wifi = config(
            &root,
            "etc/NetworkManager/system-connections/wifi",
            &wifi_text,
        );
        let notes = fix_network(&root, &sys).unwrap().join("\n");
        assert_eq!(
            fs::read_to_string(wired).unwrap(),
            text.replace(OLD, &new_mac())
        );
        assert_eq!(fs::read_to_string(wifi).unwrap(), wifi_text);
        assert!(!notes.contains("private-test-password"));
    }
}
