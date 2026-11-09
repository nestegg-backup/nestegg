//! Proof that the user really holds a copy of the encryption key.
//!
//! Backups are worthless without the key, so `nestegg key confirm-exported`
//! does not take the user's word: it needs the copy itself (a key file or a
//! saved paper key) or the check code printed on the paper key. This module
//! holds the pure parts: the check code, the paper key header, extracting a
//! key from a copy, comparing key material and deciding whether a copy lies
//! on a disk that belongs to this computer. See
//! `docs/decisions/0018-key-export-must-be-proven.md`.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The block `proxmox-backup-client key paperkey` writes around the key.
/// Everything we add to a paper key stays outside these two lines, because
/// readers of paper keys (the rescue stick) only look between them.
pub const BEGIN_MARKER: &str = "-----BEGIN PROXMOX BACKUP KEY-----";
pub const END_MARKER: &str = "-----END PROXMOX BACKUP KEY-----";

const CHECK_PREFIX: &str = "nestegg-paper-check:";

/// Check code of a key: the first 8 hex digits (upper case) of
/// `SHA-256("nestegg-paper-check:" + data)`, shown as `XXXX-XXXX`. `data` is
/// the `data` field of the key file. The code only ever appears on the paper
/// key itself, so typing it proves that the user holds that sheet.
pub fn check_code(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CHECK_PREFIX.as_bytes());
    hasher.update(data.as_bytes());
    let hex: String = hasher
        .finalize()
        .iter()
        .take(4)
        .map(|b| format!("{b:02X}"))
        .collect();
    format!("{}-{}", &hex[..4], &hex[4..])
}

/// Bring user input into the `XXXX-XXXX` form. Accepts lower case, spaces
/// and a missing hyphen; anything else is `None`.
pub fn normalize_check_code(input: &str) -> Option<String> {
    let digits: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<String>()
        .to_ascii_uppercase();
    if digits.len() != 8 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("{}-{}", &digits[..4], &digits[4..]))
}

/// The part of a key that decides whether it opens the backups: the key
/// material (`data`) and how it is protected (`kdf`). Timestamps, the
/// fingerprint and the layout of the file do not matter, so a pretty-printed
/// paper key equals the compact key file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMaterial {
    data: String,
    kdf: Value,
}

impl KeyMaterial {
    /// From the JSON of a key file.
    pub fn from_key_json(json: &str) -> Result<KeyMaterial> {
        let value: Value = serde_json::from_str(json).context("this is not a key (not JSON)")?;
        let data = value["data"]
            .as_str()
            .filter(|d| !d.is_empty())
            .context("this is not a key (no `data` field)")?;
        Ok(KeyMaterial {
            data: data.to_string(),
            kdf: value["kdf"].clone(),
        })
    }

    /// From a copy the user holds: a key file, or a paper key saved as text
    /// or HTML (the JSON between the BEGIN and END lines).
    pub fn from_copy(bytes: &[u8]) -> Result<KeyMaterial> {
        KeyMaterial::from_key_json(&key_json_of_copy(bytes)?)
    }

    /// Check code printed on the paper key of this key.
    pub fn check_code(&self) -> String {
        check_code(&self.data)
    }
}

/// The key JSON inside a copy of the key: the whole file for a key file, the
/// block between the BEGIN and END lines for a saved paper key. The result
/// is checked to be a key.
pub fn key_json_of_copy(bytes: &[u8]) -> Result<String> {
    let text = String::from_utf8_lossy(bytes);
    let json = match extract_key_block(&text) {
        Some(json) => json,
        None if text.contains(BEGIN_MARKER) => {
            bail!("this paper key has no {END_MARKER} line")
        }
        None => text.trim().to_string(),
    };
    KeyMaterial::from_key_json(&json)
        .map_err(|e| anyhow::anyhow!("no key found in this file ({e:#})"))?;
    Ok(json)
}

/// The text between the BEGIN and END lines, with HTML entities resolved
/// (a paper key saved as HTML). `None` if either marker is missing.
pub fn extract_key_block(text: &str) -> Option<String> {
    let start = text.find(BEGIN_MARKER)? + BEGIN_MARKER.len();
    let end = start + text[start..].find(END_MARKER)?;
    Some(unescape_html(text[start..end].trim()))
}

fn unescape_html(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x22;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// What the header at the top of a paper key says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaperHeader {
    pub backup_id: String,
    pub hostname: String,
    /// `YYYY-MM-DD`.
    pub date: String,
    pub server: String,
    pub datastore: String,
    pub fingerprint: String,
    pub check_code: String,
}

impl PaperHeader {
    fn computer(&self) -> String {
        if self.backup_id == self.hostname {
            self.backup_id.clone()
        } else {
            format!("{} (host name {})", self.backup_id, self.hostname)
        }
    }

    fn rows(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Computer", self.computer()),
            ("Date", self.date.clone()),
            ("PBS server", self.server.clone()),
            ("Datastore", self.datastore.clone()),
            ("Key fingerprint", self.fingerprint.clone()),
            ("Check code", self.check_code.clone()),
        ]
    }
}

const HOW_TO: [&str; 3] = [
    "To restore, save everything between the BEGIN and END lines below as a text file \
     (or scan the QR code and save its text), for example as key.json.",
    "Then give that file to the restore: `nestegg restore ... --keyfile key.json`, \
     or to the rescue stick when it asks for the key.",
    "Keep this sheet away from this computer. The check code proves you hold it: \
     `nestegg key confirm-exported --paper-code <code>`.",
];

/// Header in front of a text paper key: plain lines, then a blank line.
pub fn text_header(header: &PaperHeader) -> String {
    let mut out = String::from("nestegg paper key\n=================\n\n");
    for (label, value) in header.rows() {
        out.push_str(&format!("{:<18}{}\n", format!("{label}:"), value));
    }
    out.push('\n');
    for (i, line) in HOW_TO.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, line));
    }
    out.push('\n');
    out
}

/// Header for the start of the body of an HTML paper key.
pub fn html_header(header: &PaperHeader) -> String {
    let mut out = String::from(
        "<div style=\"page-break-inside: avoid; font-family: sans-serif; margin-bottom: 1.5em\">\n\
         <h1>nestegg paper key</h1>\n<table style=\"border-collapse: collapse\">\n",
    );
    for (label, value) in header.rows() {
        let value = escape_html(&value);
        let value = if label == "Check code" {
            format!("<b style=\"font-size: 140%; letter-spacing: 0.1em\">{value}</b>")
        } else {
            value
        };
        out.push_str(&format!(
            "<tr><td style=\"padding: 0 1em 0 0\">{}:</td><td>{value}</td></tr>\n",
            escape_html(label)
        ));
    }
    out.push_str("</table>\n<ol>\n");
    for line in HOW_TO {
        out.push_str(&format!("<li>{}</li>\n", escape_html(line)));
    }
    out.push_str("</ol>\n</div>\n");
    out
}

/// Our header followed by the unmodified text output of the client.
pub fn compose_text(header: &PaperHeader, client_output: &[u8]) -> Vec<u8> {
    let mut out = text_header(header).into_bytes();
    out.extend_from_slice(client_output);
    out
}

/// The client's HTML with our header block inserted at the start of `<body>`.
pub fn compose_html(header: &PaperHeader, client_output: &[u8]) -> Result<Vec<u8>> {
    let html = String::from_utf8_lossy(client_output);
    let body = html
        .find("<body")
        .and_then(|at| html[at..].find('>').map(|close| at + close + 1))
        .context("unexpected paper key output of proxmox-backup-client (no <body>)")?;
    let mut out = String::with_capacity(html.len() + 2048);
    out.push_str(&html[..body]);
    out.push('\n');
    out.push_str(&html_header(header));
    out.push_str(&html[body..]);
    Ok(out.into_bytes())
}

/// Whether `key paperkey` may write to stdout: a QR code scrolling past in
/// a terminal is not a backup of the key.
pub fn paperkey_stdout_allowed(has_output_file: bool, stdout_is_terminal: bool) -> bool {
    has_output_file || !stdout_is_terminal
}

/// What the device behind a file looks like, one entry per layer
/// (partition, encryption layer, disk).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceFacts {
    pub removable: bool,
    pub hotplug: bool,
    /// `usb`, `sata`, `nvme`, ... as `lsblk` reports it.
    pub transport: Option<String>,
}

impl DeviceFacts {
    fn is_external(&self) -> bool {
        let transport = self.transport.as_deref().unwrap_or("");
        // Internal buses report "hotplug" when the controller supports
        // hot-plugging, which says nothing about the disk being removable.
        let internal_bus = matches!(transport, "sata" | "ata" | "nvme" | "sas" | "mmc");
        self.removable || matches!(transport, "usb" | "ieee1394") || (self.hotplug && !internal_bus)
    }
}

/// Where a copy of the key lies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// On a removable or hot-plugged medium (USB stick, card).
    Removable,
    /// On a disk that belongs to this computer.
    Internal,
    /// Not decidable (RAM disk, network share, overlay file system, no
    /// `findmnt`/`lsblk`). Treated like [`Location::Internal`]: the user
    /// has to state that another copy exists elsewhere.
    Unknown(String),
}

impl Location {
    /// A copy here can be lost together with this computer (or is not known
    /// to be safe), so it needs the user's statement.
    pub fn needs_statement(&self) -> bool {
        !matches!(self, Location::Removable)
    }
}

/// Decide from the layers behind a file. One removable layer is enough
/// (a LUKS volume on a USB stick has the stick below it).
pub fn classify_devices(chain: &[DeviceFacts]) -> Location {
    if chain.is_empty() {
        Location::Unknown("the file is not on a block device".into())
    } else if chain.iter().any(DeviceFacts::is_external) {
        Location::Removable
    } else {
        Location::Internal
    }
}

/// Parse `lsblk -P -o RM,HOTPLUG,TRAN` (lines of `KEY="value"` pairs).
pub fn parse_lsblk_pairs(output: &str) -> Vec<DeviceFacts> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut facts = DeviceFacts::default();
            for (key, value) in lsblk_pairs(line) {
                match key.as_str() {
                    "RM" => facts.removable = value == "1" || value == "true",
                    "HOTPLUG" => facts.hotplug = value == "1" || value == "true",
                    "TRAN" if !value.is_empty() => facts.transport = Some(value),
                    _ => {}
                }
            }
            facts
        })
        .collect()
}

fn lsblk_pairs(line: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut rest = line.trim();
    while let Some(eq) = rest.find("=\"") {
        let key = rest[..eq].trim().to_string();
        let value_start = eq + 2;
        let Some(len) = rest[value_start..].find('"') else {
            break;
        };
        pairs.push((key, rest[value_start..value_start + len].to_string()));
        rest = rest[value_start + len + 1..].trim_start();
    }
    pairs
}

/// The block device from `findmnt -n -o SOURCE`: `/dev/sda2`, and for a
/// btrfs subvolume `/dev/sda2[/@home]` without the subvolume.
pub fn parse_findmnt_source(output: &str) -> Option<String> {
    let line = output.lines().next()?.trim();
    let device = line.split('[').next()?.trim();
    device.starts_with("/dev/").then(|| device.to_string())
}

/// Find out where `path` lies. Never fails: what cannot be found out is
/// [`Location::Unknown`].
pub fn locate(path: &Path) -> Location {
    let source = Command::new("findmnt")
        .args(["-n", "-o", "SOURCE", "-T"])
        .arg(path)
        .stdin(Stdio::null())
        .output();
    let source = match source {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(_) => return Location::Unknown("findmnt does not know the file".into()),
        Err(e) => return Location::Unknown(format!("cannot run findmnt: {e}")),
    };
    let Some(device) = parse_findmnt_source(&source) else {
        return Location::Unknown(format!(
            "the file is on {}, which is not a disk",
            source.trim()
        ));
    };
    // `-s` adds the devices below (encryption layers, partitions' disks).
    let chain = Command::new("lsblk")
        .args(["-s", "-n", "-P", "-o", "RM,HOTPLUG,TRAN"])
        .arg(&device)
        .stdin(Stdio::null())
        .output();
    match chain {
        Ok(o) if o.status.success() => {
            classify_devices(&parse_lsblk_pairs(&String::from_utf8_lossy(&o.stdout)))
        }
        Ok(_) => Location::Unknown(format!("lsblk does not know {device}")),
        Err(e) => Location::Unknown(format!("cannot run lsblk: {e}")),
    }
}

/// What `confirm-exported --file` does about a copy on this computer's disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalCopy {
    /// Removable medium, or the user already stated it with
    /// `--accept-local-copy`.
    Accept,
    /// A terminal is available: explain and ask for `yes`.
    Ask,
    /// No terminal and no `--accept-local-copy`.
    Refuse,
}

pub fn decide_local_copy(location: &Location, accept_flag: bool, terminal: bool) -> LocalCopy {
    if !location.needs_statement() || accept_flag {
        LocalCopy::Accept
    } else if terminal {
        LocalCopy::Ask
    } else {
        LocalCopy::Refuse
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = r#"{"kdf":null,"created":"2026-11-09T20:54:47+00:00","modified":"2026-11-09T20:54:47+00:00","data":"TkVTVEVHRy1FWEFNUExFLUtFWS1OT1QtUkVBTC0wMDE=","fingerprint":"ff:ed:94"}"#;

    fn pretty(key: &str) -> String {
        let v: Value = serde_json::from_str(key).unwrap();
        serde_json::to_string_pretty(&v).unwrap()
    }

    fn header() -> PaperHeader {
        PaperHeader {
            backup_id: "desktop".into(),
            hostname: "alice-pc".into(),
            date: "2026-11-09".into(),
            server: "pbs.example.com:8007".into(),
            datastore: "store1".into(),
            fingerprint: "ff:ed:94".into(),
            check_code: "ABCD-0123".into(),
        }
    }

    #[test]
    fn check_code_is_the_start_of_a_prefixed_sha256() {
        // sha256("nestegg-paper-check:abc")
        let mut hasher = Sha256::new();
        hasher.update(b"nestegg-paper-check:abc");
        let hex: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect();
        assert_eq!(check_code("abc"), format!("{}-{}", &hex[..4], &hex[4..8]));
        let code = check_code("abc");
        assert_eq!(code.len(), 9);
        assert_eq!(code, code.to_ascii_uppercase());
        assert_ne!(check_code("abc"), check_code("abd"));
        // Known answer, so the format cannot drift unnoticed.
        assert_eq!(check_code(""), {
            let h = Sha256::digest(b"nestegg-paper-check:");
            format!("{:02X}{:02X}-{:02X}{:02X}", h[0], h[1], h[2], h[3])
        });
    }

    #[test]
    fn check_code_input_is_forgiving_but_strict_about_the_digits() {
        assert_eq!(normalize_check_code("abcd-0123").unwrap(), "ABCD-0123");
        assert_eq!(normalize_check_code(" ab cd 0123\n").unwrap(), "ABCD-0123");
        assert_eq!(normalize_check_code("ABCD0123").unwrap(), "ABCD-0123");
        assert!(normalize_check_code("ABCD-012").is_none());
        assert!(normalize_check_code("ABCD-01234").is_none());
        assert!(normalize_check_code("ABCD-012G").is_none());
        assert!(normalize_check_code("").is_none());
    }

    #[test]
    fn key_material_ignores_layout_and_timestamps() {
        let compact = KeyMaterial::from_key_json(KEY).unwrap();
        let spaced = KeyMaterial::from_key_json(&pretty(KEY)).unwrap();
        assert_eq!(compact, spaced);
        let touched = KEY.replace("2026-11-09T20:54:47+00:00", "2027-01-01T00:00:00+00:00");
        assert_eq!(compact, KeyMaterial::from_key_json(&touched).unwrap());
        assert_eq!(compact.check_code(), spaced.check_code());
    }

    #[test]
    fn key_material_differs_for_other_data_or_protection() {
        let key = KeyMaterial::from_key_json(KEY).unwrap();
        let other_data = KEY.replace("TkVTVEVHRy1F", "TkVTVEVHRy1G");
        assert_ne!(key, KeyMaterial::from_key_json(&other_data).unwrap());
        let protected = KEY.replace(
            r#""kdf":null"#,
            r#""kdf":{"Scrypt":{"n":65536,"r":8,"p":1,"salt":"AA=="}}"#,
        );
        assert_ne!(key, KeyMaterial::from_key_json(&protected).unwrap());
        // A missing kdf is the same as null.
        let no_kdf = KEY.replace(r#""kdf":null,"#, "");
        assert_eq!(key, KeyMaterial::from_key_json(&no_kdf).unwrap());
    }

    #[test]
    fn things_that_are_not_keys_are_rejected() {
        assert!(KeyMaterial::from_key_json("not json").is_err());
        assert!(KeyMaterial::from_key_json(r#"{"fp":"aa"}"#).is_err());
        assert!(KeyMaterial::from_key_json(r#"{"data":""}"#).is_err());
        assert!(KeyMaterial::from_copy(b"just some text").is_err());
        let torn = format!("{BEGIN_MARKER}\n{KEY}\n");
        let err = KeyMaterial::from_copy(torn.as_bytes()).unwrap_err();
        assert!(format!("{err:#}").contains("END"), "{err:#}");
        let damaged = format!("{BEGIN_MARKER}\n{{\"data\": \n{END_MARKER}\n");
        assert!(KeyMaterial::from_copy(damaged.as_bytes()).is_err());
    }

    #[test]
    fn key_is_found_in_a_key_file_and_in_text_and_html_paper_keys() {
        let expect = KeyMaterial::from_key_json(KEY).unwrap();
        // Key file, with and without trailing newline.
        assert_eq!(KeyMaterial::from_copy(KEY.as_bytes()).unwrap(), expect);
        assert_eq!(
            KeyMaterial::from_copy(format!("{KEY}\n").as_bytes()).unwrap(),
            expect
        );
        // Text paper key as the client writes it, behind our header.
        let client_text = format!(
            "{BEGIN_MARKER}\n{}\n{END_MARKER}\n\u{2588}\u{2580}\u{2580} qr code\n",
            pretty(KEY)
        );
        let text = compose_text(&header(), client_text.as_bytes());
        assert_eq!(KeyMaterial::from_copy(&text).unwrap(), expect);
        // HTML paper key.
        let client_html = format!(
            "<!DOCTYPE html>\n<html>\n<head><title>Proxmox Backup Paperkey</title></head>\n<body>\n\
             <div style=\"page-break-inside: avoid\">\n<p>\n{BEGIN_MARKER}\n{}\n{END_MARKER}\n</p>\n\
             <center><img src=\"data:image/svg+xml;base64,AAAA\"></center>\n</div>\n</body>\n</html>\n",
            pretty(KEY)
        );
        let html = compose_html(&header(), client_html.as_bytes()).unwrap();
        assert_eq!(KeyMaterial::from_copy(&html).unwrap(), expect);
        // Entities, as after saving the page from a browser.
        let escaped = client_html.replace('"', "&quot;");
        let escaped = escaped.replace("<div style=&quot;", "<div style=\"");
        assert_eq!(KeyMaterial::from_copy(escaped.as_bytes()).unwrap(), expect);
    }

    #[test]
    fn text_header_stays_outside_the_key_block_and_shows_the_facts() {
        let client_text = format!("{BEGIN_MARKER}\n{}\n{END_MARKER}\nQR\n", pretty(KEY));
        let out = String::from_utf8(compose_text(&header(), client_text.as_bytes())).unwrap();
        let begin = out.find(BEGIN_MARKER).unwrap();
        let head = &out[..begin];
        for needle in [
            "nestegg paper key",
            "desktop (host name alice-pc)",
            "2026-11-09",
            "pbs.example.com:8007",
            "store1",
            "ff:ed:94",
            "ABCD-0123",
        ] {
            assert!(head.contains(needle), "{needle} missing in\n{head}");
        }
        // Exactly one block, and the client output is untouched after our header.
        assert_eq!(out.matches(BEGIN_MARKER).count(), 1);
        assert!(out.ends_with(&client_text));
        assert_eq!(
            head.lines()
                .filter(|l| l.starts_with(|c: char| c.is_ascii_digit()))
                .count(),
            3
        );
    }

    #[test]
    fn html_header_is_inserted_at_the_start_of_the_body() {
        let client = "<html><head></head><body class=\"x\">\n<p>\nBLOCK\n</p>\n</body></html>\n";
        let out = String::from_utf8(compose_html(&header(), client.as_bytes()).unwrap()).unwrap();
        let body = out.find("<body class=\"x\">").unwrap();
        let title = out.find("nestegg paper key").unwrap();
        let block = out.find("BLOCK").unwrap();
        assert!(body < title && title < block);
        assert!(out.contains("ABCD-0123"));
        assert!(out.ends_with("</body></html>\n"));
        assert!(compose_html(&header(), b"no markup here").is_err());
    }

    #[test]
    fn html_header_escapes_what_it_shows() {
        let mut h = header();
        h.backup_id = "<script>".into();
        h.hostname = "<script>".into();
        let html = html_header(&h);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn paper_key_on_a_terminal_needs_an_output_file() {
        assert!(!paperkey_stdout_allowed(false, true));
        assert!(paperkey_stdout_allowed(true, true));
        assert!(paperkey_stdout_allowed(false, false));
        assert!(paperkey_stdout_allowed(true, false));
    }

    fn facts(removable: bool, hotplug: bool, tran: Option<&str>) -> DeviceFacts {
        DeviceFacts {
            removable,
            hotplug,
            transport: tran.map(String::from),
        }
    }

    #[test]
    fn location_follows_removable_hotplug_and_transport() {
        let internal = [facts(false, false, None), facts(false, false, Some("nvme"))];
        assert_eq!(classify_devices(&internal), Location::Internal);
        // SATA controllers often flag every port as hot-plug.
        let sata = [facts(false, true, Some("sata"))];
        assert_eq!(classify_devices(&sata), Location::Internal);
        let stick = [facts(true, true, Some("usb"))];
        assert_eq!(classify_devices(&stick), Location::Removable);
        let usb_disk = [facts(false, false, Some("usb"))];
        assert_eq!(classify_devices(&usb_disk), Location::Removable);
        let card = [facts(true, false, None)];
        assert_eq!(classify_devices(&card), Location::Removable);
        let hotplug_only = [facts(false, true, None)];
        assert_eq!(classify_devices(&hotplug_only), Location::Removable);
        // LUKS on a stick: the layer above is plain, the disk below is USB.
        let luks_on_stick = [facts(false, false, None), facts(true, true, Some("usb"))];
        assert_eq!(classify_devices(&luks_on_stick), Location::Removable);
        assert!(matches!(classify_devices(&[]), Location::Unknown(_)));
    }

    #[test]
    fn lsblk_and_findmnt_output_is_parsed() {
        let out = "RM=\"0\" HOTPLUG=\"0\" TRAN=\"\"\nRM=\"1\" HOTPLUG=\"1\" TRAN=\"usb\"\n";
        let chain = parse_lsblk_pairs(out);
        assert_eq!(
            chain,
            vec![facts(false, false, None), facts(true, true, Some("usb"))]
        );
        assert_eq!(classify_devices(&chain), Location::Removable);
        assert_eq!(
            parse_lsblk_pairs("RM=\"0\" HOTPLUG=\"0\" TRAN=\"sata\"\n"),
            vec![facts(false, false, Some("sata"))]
        );
        assert!(parse_lsblk_pairs("\n").is_empty());
        assert_eq!(parse_findmnt_source("/dev/sda2\n").unwrap(), "/dev/sda2");
        assert_eq!(
            parse_findmnt_source("/dev/nvme0n1p2[/@home]\n").unwrap(),
            "/dev/nvme0n1p2"
        );
        assert!(parse_findmnt_source("overlay\n").is_none());
        assert!(parse_findmnt_source("tmpfs\n").is_none());
        assert!(parse_findmnt_source("").is_none());
    }

    #[test]
    fn a_copy_on_this_computers_disk_needs_the_users_statement() {
        let internal = Location::Internal;
        let unknown = Location::Unknown("overlay".into());
        let stick = Location::Removable;
        assert_eq!(decide_local_copy(&stick, false, false), LocalCopy::Accept);
        assert_eq!(decide_local_copy(&stick, false, true), LocalCopy::Accept);
        assert_eq!(
            decide_local_copy(&internal, false, false),
            LocalCopy::Refuse
        );
        assert_eq!(decide_local_copy(&internal, false, true), LocalCopy::Ask);
        assert_eq!(decide_local_copy(&internal, true, false), LocalCopy::Accept);
        assert_eq!(decide_local_copy(&internal, true, true), LocalCopy::Accept);
        assert_eq!(decide_local_copy(&unknown, false, false), LocalCopy::Refuse);
        assert_eq!(decide_local_copy(&unknown, false, true), LocalCopy::Ask);
        assert_eq!(decide_local_copy(&unknown, true, false), LocalCopy::Accept);
    }
}
