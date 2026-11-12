//! Inventory belongs to the mounted computer, never to the live system.
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};

use super::offline_storage::{read_root, text_root};
use crate::layout::Layout;
use crate::stick::capture;

pub fn generate(root: &Path, layout: &Layout, out: &Path, source_tree: &str) -> Result<()> {
    crate::secrets::create_private_dir(out)?;
    let admin = out.join("dpkg-admin");
    fs::create_dir(&admin)?;
    // Copy through openat2 first: a status symlink on the offline computer
    // must not make the host dpkg read the live system's files.
    fs::write(
        admin.join("status"),
        read_root(root, "/var/lib/dpkg/status")
            .context("offline system has no Debian package database")?,
    )?;
    let selections = capture(
        Command::new("dpkg")
            .arg(format!("--admindir={}", admin.display()))
            .arg("--get-selections"),
    )?;
    ensure!(
        !selections.trim().is_empty(),
        "offline Debian package list is empty"
    );
    fs::write(out.join("dpkg-selections.txt"), format!("{selections}\n"))?;
    fs::remove_dir_all(admin)?;
    let services = capture(Command::new("systemctl").arg("--root").arg(root).args([
        "list-unit-files",
        "--state=enabled",
        "--no-pager",
    ]))?;
    fs::write(out.join("systemd-enabled.txt"), services)?;
    for (source, target, required) in [
        ("/etc/os-release", "os-release", true),
        ("/etc/fstab", "fstab", true),
        ("/etc/crypttab", "crypttab", false),
        ("/var/lib/apt/extended_states", "apt-extended-states", false),
        ("/etc/apt/sources.list", "apt-sources.txt", false),
        ("/var/lib/snapd/state.json", "snap-state.json", false),
        ("/var/lib/flatpak/repo/config", "flatpak-repo.conf", false),
    ] {
        match read_root(root, source) {
            Ok(bytes) => fs::write(out.join(target), bytes)?,
            Err(e) if required => return Err(e),
            Err(_) => {}
        }
    }
    // Deployment metadata is the offline equivalent of `flatpak list`.
    // No Flatpak/Snap daemon from the live system is queried.
    let app_root = root.join("var/lib/flatpak/app");
    if let Ok(base) = fs::canonicalize(&app_root) {
        ensure!(
            base.starts_with(root),
            "Flatpak state directory escapes offline root"
        );
        let mut metadata = String::new();
        for app in fs::read_dir(base)? {
            let app = app?;
            if !app.file_type()?.is_dir() {
                continue;
            }
            for arch in fs::read_dir(app.path())? {
                let arch = arch?;
                if !arch.file_type()?.is_dir() {
                    continue;
                }
                for branch in fs::read_dir(arch.path())? {
                    let branch = branch?;
                    if !branch.file_type()?.is_dir() {
                        continue;
                    }
                    let logical = branch.path().strip_prefix(root)?.join("active/metadata");
                    if let Ok(text) = text_root(root, &format!("/{}", logical.display())) {
                        metadata.push_str(&format!("\n# {}\n{text}\n", logical.display()));
                    }
                }
            }
        }
        fs::write(out.join("flatpak-deployments.txt"), metadata)?;
    }
    fs::write(
        out.join(crate::layout::LAYOUT_FILE),
        serde_json::to_vec_pretty(layout)?,
    )?;
    fs::write(out.join("boot-mode.txt"), format!("{}\n", layout.boot_mode))?;
    // Only the selected source disk, not the live stick or unrelated drives.
    fs::write(out.join("lsblk.json"), source_tree)?;
    fs::write(
        out.join("README.txt"),
        "Offline inventory from the mounted computer.\nSnap and Flatpak are recorded from their state/deployment files; no live daemon was queried.\n",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_offline_dpkg_database_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let layout = crate::layout::tests::btrfs_layout();
        assert!(generate(d.path(), &layout, &d.path().join("inventory"), "{}").is_err());
    }
    #[test]
    fn package_and_application_inventory_comes_from_offline_files() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("root");
        for path in [
            "var/lib/dpkg",
            "var/lib/snapd",
            "etc",
            "var/lib/flatpak/repo",
            "var/lib/flatpak/app/org.example.App/x86_64/stable/deployment",
        ] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        fs::write(root.join("var/lib/dpkg/status"), "Package: offline-fixture\nStatus: install ok installed\nArchitecture: all\nVersion: 1\nMaintainer: Example <example@example.com>\nDescription: Offline fixture\n\n").unwrap();
        fs::write(root.join("etc/os-release"), "ID=offline-fixture\n").unwrap();
        fs::write(root.join("etc/fstab"), "UUID=fixture / ext4 defaults 0 1\n").unwrap();
        fs::write(root.join("var/lib/snapd/state.json"), "{\"offline\":true}").unwrap();
        fs::write(
            root.join("var/lib/flatpak/repo/config"),
            "[core]\nrepo_version=1\n",
        )
        .unwrap();
        fs::write(
            root.join("var/lib/flatpak/app/org.example.App/x86_64/stable/deployment/metadata"),
            "[Application]\nname=org.example.App\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "deployment",
            root.join("var/lib/flatpak/app/org.example.App/x86_64/stable/active"),
        )
        .unwrap();
        let units = root.join("etc/systemd/system");
        fs::create_dir_all(units.join("multi-user.target.wants")).unwrap();
        fs::write(units.join("offline-fixture.service"), "[Unit]\nDescription=Offline fixture\n[Service]\nType=oneshot\nExecStart=/bin/true\n[Install]\nWantedBy=multi-user.target\n").unwrap();
        std::os::unix::fs::symlink(
            "../offline-fixture.service",
            units.join("multi-user.target.wants/offline-fixture.service"),
        )
        .unwrap();
        let out = d.path().join("inventory");
        generate(
            &root,
            &crate::layout::tests::btrfs_layout(),
            &out,
            "{\"blockdevices\":[]}",
        )
        .unwrap();
        let selections = fs::read_to_string(out.join("dpkg-selections.txt")).unwrap();
        assert_eq!(
            selections.split_whitespace().collect::<Vec<_>>(),
            ["offline-fixture", "install"]
        );
        assert_eq!(
            fs::read_to_string(out.join("snap-state.json")).unwrap(),
            "{\"offline\":true}"
        );
        assert!(
            fs::read_to_string(out.join("flatpak-deployments.txt"))
                .unwrap()
                .contains("name=org.example.App")
        );
        assert_eq!(
            fs::read_to_string(out.join("lsblk.json")).unwrap(),
            "{\"blockdevices\":[]}"
        );
        assert!(
            fs::read_to_string(out.join("systemd-enabled.txt"))
                .unwrap()
                .contains("offline-fixture.service")
        );
        assert!(!out.join("dpkg-admin").exists());
    }
}
