//! Adjust the restored configuration to the new disk: `/etc/fstab`,
//! `/etc/crypttab`, resume device and kernel command line. Pure text
//! transformations; every change is reported.

use std::collections::{HashMap, HashSet};

use super::plan::Plan;
use crate::layout::{Layout, MountRole};

/// Name of the LUKS mapping of the new root file system.
pub const LUKS_NAME: &str = "nestegg_root";

/// Identifiers of the newly created file systems.
#[derive(Debug, Clone, Default)]
pub struct NewIds {
    /// Mount point → UUID of the new file system (btrfs subvolumes share the
    /// UUID of the root file system).
    pub fs_uuid: HashMap<String, String>,
    pub swap_uuid: Option<String>,
    pub luks_uuid: Option<String>,
}

impl NewIds {
    /// Old UUID → new UUID, for references in configuration files.
    pub fn uuid_map(&self, layout: &Layout) -> Vec<(String, String)> {
        let mut map = Vec::new();
        for m in &layout.mounts {
            if let (Some(old), Some(new)) = (&m.uuid, self.fs_uuid.get(&m.point))
                && !map.iter().any(|(o, _): &(String, String)| o == old)
            {
                map.push((old.clone(), new.clone()));
            }
        }
        if let Some(new) = &self.swap_uuid {
            for s in layout.swap.iter().filter(|s| s.kind == "partition") {
                if let Some(old) = &s.uuid {
                    map.push((old.clone(), new.clone()));
                }
            }
        }
        if let (Some(new), Some(old)) = (
            &self.luks_uuid,
            layout.root().and_then(|r| r.crypt_uuid.clone()),
        ) {
            map.push((old, new.clone()));
        }
        map
    }

    fn known_new(&self) -> HashSet<&str> {
        self.fs_uuid
            .values()
            .map(String::as_str)
            .chain(self.swap_uuid.as_deref())
            .chain(self.luks_uuid.as_deref())
            .collect()
    }
}

fn is_device_spec(spec: &str) -> bool {
    ["UUID=", "PARTUUID=", "LABEL=", "PARTLABEL=", "/dev/"]
        .iter()
        .any(|p| spec.starts_with(p))
}

fn unescape(s: &str) -> String {
    s.replace("\\040", " ").replace("\\011", "\t")
}

fn add_option(options: &str, option: &str) -> String {
    if options.split(',').any(|o| o == option) {
        options.to_string()
    } else if options.is_empty() || options == "defaults" {
        format!("defaults,{option}")
    } else {
        format!("{options},{option}")
    }
}

/// Rewrite `/etc/fstab`. Returns the new text and notes for the report.
/// `map` is [`NewIds::uuid_map`]: other lines that name a re-created file
/// system by its old UUID (a second mount of a subvolume) get the new one.
pub fn rewrite_fstab(
    text: &str,
    plan: &Plan,
    ids: &NewIds,
    map: &[(String, String)],
) -> (String, Vec<String>) {
    let mut out = String::new();
    let mut notes = Vec::new();
    let mut seen_points = HashSet::new();
    let mut has_swapfile = HashSet::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        if trimmed.is_empty() || trimmed.starts_with('#') || fields.len() < 4 {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let (spec, point, fstype, options) = (fields[0], unescape(fields[1]), fields[2], fields[3]);
        let rest: Vec<&str> = fields[4..].to_vec();
        let join = |spec: &str, fstype: &str, options: &str| {
            let mut v = vec![
                spec.to_string(),
                fields[1].to_string(),
                fstype.to_string(),
                options.to_string(),
            ];
            v.extend(rest.iter().map(|s| s.to_string()));
            v.join("\t")
        };
        if fstype == "swap" {
            if spec.starts_with('/') && !spec.starts_with("/dev/") {
                has_swapfile.insert(spec.to_string());
                out.push_str(line);
            } else if let Some(uuid) = &ids.swap_uuid {
                out.push_str(&join(&format!("UUID={uuid}"), fstype, options));
                notes.push(format!("fstab: swap now uses UUID={uuid}"));
            } else {
                out.push_str(&format!(
                    "# disabled by nestegg rescue (swap partition not recreated): {line}"
                ));
                notes.push(format!("fstab: swap entry {spec} disabled"));
            }
            out.push('\n');
            continue;
        }
        if let Some(uuid) = ids.fs_uuid.get(&point) {
            seen_points.insert(point.clone());
            let fstype = if point == "/" {
                plan.root_fstype.as_str()
            } else {
                fstype
            };
            out.push_str(&join(&format!("UUID={uuid}"), fstype, options));
            notes.push(format!("fstab: {point} now uses UUID={uuid}"));
        } else if plan.merged.contains(&point) {
            out.push_str(&format!("# merged into / by nestegg rescue: {line}"));
            notes.push(format!("fstab: {point} is now part of /; entry disabled"));
        } else if let Some(new) = spec
            .strip_prefix("UUID=")
            .and_then(|old| map.iter().find(|(o, _)| o == old))
            .map(|(_, n)| n)
        {
            out.push_str(&join(&format!("UUID={new}"), fstype, options));
            notes.push(format!("fstab: {point} now uses UUID={new}"));
        } else if is_device_spec(spec) && !options.split(',').any(|o| o == "nofail") {
            out.push_str(&join(spec, fstype, &add_option(options, "nofail")));
            notes.push(format!(
                "fstab: {point} is not on the restored disk; added 'nofail' so a missing disk does not stop the boot"
            ));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if let Some(esp) = &plan.esp_point
        && !seen_points.contains(esp)
        && let Some(uuid) = ids.fs_uuid.get(esp)
    {
        out.push_str(&format!("UUID={uuid}\t{esp}\tvfat\tumask=0077\t0\t1\n"));
        notes.push(format!("fstab: added the EFI system partition at {esp}"));
    }
    if plan.encrypt
        && !seen_points.contains("/boot")
        && let Some(uuid) = ids.fs_uuid.get("/boot")
    {
        out.push_str(&format!("UUID={uuid}\t/boot\text4\tdefaults\t0\t2\n"));
        notes.push("fstab: added the unencrypted /boot partition".into());
    }
    // Subvolumes the plan adds (e.g. `@swap` for a new swap file).
    for sv in &plan.subvolumes {
        if !seen_points.contains(&sv.point)
            && let Some(uuid) = ids.fs_uuid.get(&sv.point)
        {
            out.push_str(&format!(
                "UUID={uuid}\t{}\tbtrfs\tsubvol=/{},defaults\t0\t0\n",
                sv.point, sv.name
            ));
            notes.push(format!(
                "fstab: added subvolume {} at {}",
                sv.name, sv.point
            ));
        }
    }
    for f in &plan.swap_files {
        if has_swapfile.insert(f.path.clone()) {
            out.push_str(&format!("{}\tnone\tswap\tsw\t0\t0\n", f.path));
            notes.push(format!("fstab: added swap file {}", f.path));
        }
    }
    (out, notes)
}

/// Rewrite `/etc/crypttab`: entries of the old system disk are removed, the
/// new root container is added, other containers get `nofail`.
pub fn rewrite_crypttab(text: &str, layout: &Layout, ids: &NewIds) -> (String, Vec<String>) {
    let old: HashSet<String> = layout
        .mounts
        .iter()
        .filter(|m| matches!(layout.role(m), MountRole::Root | MountRole::System))
        .filter_map(|m| m.crypt_uuid.clone())
        .chain(layout.swap.iter().filter_map(|s| s.crypt_uuid.clone()))
        .collect();
    let mut out = String::new();
    let mut notes = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        if trimmed.is_empty() || trimmed.starts_with('#') || fields.len() < 2 {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let device = fields[1];
        let refers_old = device
            .strip_prefix("UUID=")
            .is_some_and(|u| old.contains(u));
        let random_key = fields
            .get(2)
            .is_some_and(|k| k.starts_with("/dev/urandom") || k.starts_with("/dev/random"));
        if refers_old || random_key {
            out.push_str(&format!("# disabled by nestegg rescue: {line}"));
            notes.push(format!(
                "crypttab: entry {} disabled (old system disk)",
                fields[0]
            ));
        } else {
            let options = fields.get(3).copied().unwrap_or("luks");
            let mut v: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
            if v.len() < 3 {
                v.push("none".into());
            }
            let new_opts = add_option(options, "nofail");
            if v.len() < 4 {
                v.push(new_opts);
            } else {
                v[3] = new_opts;
            }
            out.push_str(&v.join("\t"));
            notes.push(format!("crypttab: added 'nofail' to {}", fields[0]));
        }
        out.push('\n');
    }
    if let Some(uuid) = &ids.luks_uuid {
        out.push_str(&format!("{LUKS_NAME}\tUUID={uuid}\tnone\tluks,discard\n"));
        notes.push(format!(
            "crypttab: added {LUKS_NAME} for the encrypted root"
        ));
    }
    (out, notes)
}

/// Replace old UUIDs, and drop resume parameters that would point nowhere.
/// Used for `/etc/default/grub`, `/etc/kernel/cmdline` and the initramfs
/// resume configuration.
pub fn replace_uuids(text: &str, map: &[(String, String)], ids: &NewIds) -> String {
    let mut s = text.to_string();
    for (old, new) in map {
        s = s.replace(old.as_str(), new.as_str());
    }
    let known = ids.known_new();
    let mut out = String::new();
    for line in s.split_inclusive('\n') {
        let (body, nl) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        // initramfs-tools: RESUME=UUID=<x>
        if let Some(value) = body.trim().strip_prefix("RESUME=") {
            let uuid = value.trim_matches('"').trim_start_matches("UUID=");
            if !known.contains(uuid) && value != "none" && value != "auto" {
                out.push_str("RESUME=none");
                out.push_str(nl);
                continue;
            }
        }
        let stale = |inner: &str| -> bool {
            if inner.starts_with("resume_offset=") {
                return true;
            }
            if let Some(v) = inner.strip_prefix("resume=") {
                let uuid = v.trim_start_matches("UUID=");
                return !known.contains(uuid);
            }
            false
        };
        // Split each token into `NAME="` (or `NAME='`), the value, and a
        // closing quote; drop stale values but keep the quoting intact.
        let mut kept: Vec<String> = Vec::new();
        let mut glue = false;
        for tok in body.split(' ') {
            let (prefix, rest) = match tok.find("=\"").or_else(|| tok.find("='")) {
                Some(i) => tok.split_at(i + 2),
                None => ("", tok),
            };
            let inner = rest.trim_end_matches(['"', '\'']);
            let suffix = &rest[inner.len()..];
            let piece = if stale(inner.trim_start_matches(['"', '\''])) {
                format!("{prefix}{suffix}")
            } else {
                tok.to_string()
            };
            if piece.is_empty() {
                continue;
            }
            match kept.last_mut() {
                // After a dropped first value: `NAME="` + next token.
                Some(last) if glue => last.push_str(&piece),
                // A lone closing quote belongs to the previous token.
                Some(last) if piece.chars().all(|c| c == '"' || c == '\'') => last.push_str(&piece),
                _ => kept.push(piece.clone()),
            }
            glue = !prefix.is_empty() && piece == prefix;
        }
        let joined = kept.join(" ");
        out.push_str(&joined);
        out.push_str(nl);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::tests::btrfs_layout;
    use crate::rescue::plan::{Target, make_plan};

    fn ids() -> NewIds {
        let mut fs_uuid = HashMap::new();
        for p in ["/", "/home", "/.snapshots"] {
            fs_uuid.insert(p.to_string(), "new-root".to_string());
        }
        fs_uuid.insert("/boot/efi".into(), "NEW-ESP".into());
        NewIds {
            fs_uuid,
            swap_uuid: Some("new-swap".into()),
            luks_uuid: None,
        }
    }

    fn plan() -> Plan {
        make_plan(
            &btrfs_layout(),
            &Target {
                disk: "/dev/vda".into(),
                size_bytes: 200 << 30,
                uefi: true,
                archive_bytes: None,
            },
            false,
        )
        .unwrap()
    }

    const FSTAB: &str = "\
# /etc/fstab
UUID=1111 /               btrfs   defaults,subvol=@ 0       1
UUID=1111 /home           btrfs   defaults,subvol=@home 0       2
UUID=1111 /.snapshots     btrfs   subvol=@snapshots 0 0
UUID=AAAA-BBBB  /boot/efi       vfat    umask=0077      0       1
UUID=3333 none            swap    sw              0       0
UUID=2222 /srv/data ext4 defaults 0 2
tmpfs /tmp tmpfs defaults 0 0
";

    #[test]
    fn fstab() {
        let (text, notes) = rewrite_fstab(FSTAB, &plan(), &ids(), &[]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "UUID=new-root\t/\tbtrfs\tdefaults,subvol=@\t0\t1");
        assert_eq!(
            lines[2],
            "UUID=new-root\t/home\tbtrfs\tdefaults,subvol=@home\t0\t2"
        );
        assert!(lines[3].starts_with("UUID=new-root\t/.snapshots"));
        assert_eq!(lines[4], "UUID=NEW-ESP\t/boot/efi\tvfat\tumask=0077\t0\t1");
        assert_eq!(lines[5], "UUID=new-swap\tnone\tswap\tsw\t0\t0");
        assert_eq!(
            lines[6],
            "UUID=2222\t/srv/data\text4\tdefaults,nofail\t0\t2"
        );
        assert_eq!(lines[7], "tmpfs /tmp tmpfs defaults 0 0");
        assert_eq!(lines.len(), 8);
        assert!(notes.iter().any(|n| n.contains("nofail")));
    }

    #[test]
    fn fstab_second_mount_gets_the_new_uuid() {
        let map = vec![("1111".to_string(), "new-root".to_string())];
        let (text, _) = rewrite_fstab(
            "UUID=1111 /srv/home btrfs subvol=@home 0 0\n/home/alice/x /srv/x none bind 0 0\n",
            &plan(),
            &ids(),
            &map,
        );
        assert!(text.starts_with(
            "UUID=new-root\t/srv/home\tbtrfs\tsubvol=@home\t0\t0\n/home/alice/x /srv/x none bind 0 0\n"
        ));
    }

    #[test]
    fn fstab_merges_and_adds_missing_esp() {
        let mut p = plan();
        p.merged = vec!["/home".into()];
        let mut i = ids();
        i.fs_uuid.remove("/home");
        let (text, _) = rewrite_fstab(
            "UUID=x / ext4 defaults 0 1\nUUID=y /home ext4 defaults 0 2\n/swapfile none swap sw 0 0\n",
            &p,
            &i,
            &[],
        );
        assert!(text.contains("# merged into / by nestegg rescue: UUID=y /home"));
        assert!(text.contains("UUID=NEW-ESP\t/boot/efi\tvfat"));
        assert!(text.contains("/swapfile none swap sw 0 0"));
    }

    #[test]
    fn crypttab() {
        let mut l = btrfs_layout();
        l.mounts[0].crypt_uuid = Some("old-luks".into());
        let mut i = ids();
        i.luks_uuid = Some("new-luks".into());
        let (text, _) = rewrite_crypttab(
            "nvme0n1p3_crypt UUID=old-luks none luks,discard\ndata UUID=d /root/data.key luks\ncryptswap /dev/sda3 /dev/urandom swap\n",
            &l,
            &i,
        );
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("# disabled by nestegg rescue"));
        assert_eq!(lines[1], "data\tUUID=d\t/root/data.key\tluks,nofail");
        assert!(lines[2].starts_with("# disabled"));
        assert_eq!(lines[3], "nestegg_root\tUUID=new-luks\tnone\tluks,discard");
    }

    #[test]
    fn uuids_and_resume() {
        let l = btrfs_layout();
        let i = ids();
        let map = i.uuid_map(&l);
        assert!(map.contains(&("3333".into(), "new-swap".into())));
        let grub = "GRUB_CMDLINE_LINUX_DEFAULT=\"quiet splash resume=UUID=3333\"\nGRUB_CMDLINE_LINUX=\"root=UUID=1111 resume=UUID=dead resume_offset=1234\"\n";
        let out = replace_uuids(grub, &map, &i);
        assert_eq!(
            out,
            "GRUB_CMDLINE_LINUX_DEFAULT=\"quiet splash resume=UUID=new-swap\"\nGRUB_CMDLINE_LINUX=\"root=UUID=new-root\"\n"
        );
        // A stale resume= as the first token after the opening quote.
        assert_eq!(
            replace_uuids("GRUB_CMDLINE_LINUX=\"resume=UUID=dead quiet\"\n", &map, &i),
            "GRUB_CMDLINE_LINUX=\"quiet\"\n"
        );
        assert_eq!(
            replace_uuids("GRUB_CMDLINE_LINUX=\"resume=UUID=dead\"\n", &map, &i),
            "GRUB_CMDLINE_LINUX=\"\"\n"
        );
        // Single quotes are kept balanced, too.
        assert_eq!(
            replace_uuids("GRUB_CMDLINE_LINUX='quiet resume=UUID=dead'\n", &map, &i),
            "GRUB_CMDLINE_LINUX='quiet'\n"
        );
        assert_eq!(
            replace_uuids("GRUB_CMDLINE_LINUX='resume=UUID=dead quiet'\n", &map, &i),
            "GRUB_CMDLINE_LINUX='quiet'\n"
        );
        assert_eq!(
            replace_uuids("GRUB_CMDLINE_LINUX=\"a resume=UUID=dead b\"\n", &map, &i),
            "GRUB_CMDLINE_LINUX=\"a b\"\n"
        );
        assert_eq!(
            replace_uuids("RESUME=UUID=3333\n", &map, &i),
            "RESUME=UUID=new-swap\n"
        );
        let mut no_swap = i.clone();
        no_swap.swap_uuid = None;
        assert_eq!(
            replace_uuids("RESUME=UUID=3333\n", &no_swap.uuid_map(&l), &no_swap),
            "RESUME=none\n"
        );
    }
}
