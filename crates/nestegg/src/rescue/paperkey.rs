//! Reading an encryption key from what a person has: a saved paper key
//! (text or html, as written by `nestegg key paperkey`), an exported key
//! file, or the one line of a printed paper key typed by hand.
//!
//! `proxmox-backup-client key paperkey` prints
//!
//! ```text
//! -----BEGIN PROXMOX BACKUP KEY-----
//! { "kdf": null, "created": "...", "modified": "...", "data": "...", "fingerprint": "..." }
//! -----END PROXMOX BACKUP KEY-----
//! <QR code>
//! ```
//!
//! Tested with the real client (4.2.8): a key file with only `created`,
//! `modified` and `data` (44 base64 characters = 32 bytes) works for
//! restoring; `kdf` and `fingerprint` may be missing, `created` and
//! `modified` must be strings. So typing the `data` line is enough.

use serde_json::{Map, Value};

pub const BEGIN: &str = "-----BEGIN PROXMOX BACKUP KEY-----";
pub const END: &str = "-----END PROXMOX BACKUP KEY-----";

/// Length of the `data` value: 32 bytes in base64, with padding.
pub const DATA_CHARS: usize = 44;
/// Used when a pasted key has no (usable) time stamps; the client only
/// needs the fields to exist.
const FALLBACK_TIME: &str = "1970-01-01T00:00:00+00:00";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyTextError {
    /// The text is not a key at all.
    #[error("{0}")]
    NotAKey(String),
    /// A key, but protected with a password (nestegg cannot use those).
    #[error(
        "this key is protected with a password (kdf {0}); nestegg needs a key without password. \
         Remove the password from a copy first: \
         proxmox-backup-client key change-passphrase --kdf none <copy of the file>"
    )]
    Protected(String),
}

/// A usable, unprotected key in the form the client reads from a key file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    /// Key file content (one line of JSON).
    pub json: String,
    /// Fingerprint, if the paper key or key file carried one.
    pub fingerprint: Option<String>,
}

/// Parse the content of a file: a paper key (text or html) or an exported
/// key file (JSON).
pub fn from_file_text(text: &str) -> Result<Key, KeyTextError> {
    let text = text.trim_start_matches('\u{feff}');
    let plain = if looks_like_html(text) {
        html_to_text(text)
    } else {
        text.to_string()
    };
    if plain.contains(BEGIN) {
        return from_block(&plain);
    }
    // An exported key file.
    match first_json_object(&plain) {
        Some(v) if plain.trim_start().starts_with('{') => from_json(&v),
        _ => Err(KeyTextError::NotAKey(
            "no paper key found (the lines -----BEGIN PROXMOX BACKUP KEY----- ... \
             -----END PROXMOX BACKUP KEY----- are missing) and not a key file"
                .into(),
        )),
    }
}

/// Parse what was typed or pasted at the console: normally only the
/// `data` line, tolerated with quotes, a comma, `"data":` in front and
/// stray blanks. Pasting the whole JSON is accepted as well.
pub fn from_typed(input: &str) -> Result<Key, KeyTextError> {
    if input.contains('{') || input.contains(BEGIN) {
        return from_file_text(input);
    }
    let value = match input.split_once("\"data\"") {
        Some((_, after)) => after.trim_start().trim_start_matches(':'),
        None => input,
    };
    let data = normalize_data(value)?;
    Ok(build(None, None, &data, None))
}

/// Clean a typed `data` value and check it: 44 base64 characters (the
/// final `=` may be forgotten) that decode to 32 bytes.
pub fn normalize_data(raw: &str) -> Result<String, KeyTextError> {
    let mut s: String = raw
        .chars()
        .filter(|c| {
            !c.is_whitespace() && !matches!(c, '"' | '\'' | ',' | '`' | '\u{201c}' | '\u{201d}')
        })
        .collect();
    if s.len() == DATA_CHARS - 1 && !s.ends_with('=') {
        s.push('=');
    }
    for (i, c) in s.chars().enumerate() {
        let ok =
            c.is_ascii_alphanumeric() || c == '+' || c == '/' || (c == '=' && i == s.len() - 1);
        if !ok {
            return Err(KeyTextError::NotAKey(format!(
                "character {} ('{c}') cannot be part of a key: only letters (upper and lower \
                 case), digits, + and / are used, and one = at the very end. Check the \
                 keyboard layout of the console (US layout).",
                i + 1
            )));
        }
    }
    if s.chars().count() != DATA_CHARS {
        return Err(KeyTextError::NotAKey(format!(
            "got {} characters, but the data line of a key has {DATA_CHARS} (ending with =)",
            s.chars().count()
        )));
    }
    match decode_base64(&s) {
        Some(bytes) if bytes.len() == 32 => Ok(s),
        _ => Err(KeyTextError::NotAKey(
            "this is not a valid key: the characters do not decode to 32 bytes".into(),
        )),
    }
}

fn from_block(text: &str) -> Result<Key, KeyTextError> {
    // Every END marker, with the text since the last BEGIN before it.
    let mut first_error = None;
    let mut ends = text.match_indices(END).map(|(i, _)| i).peekable();
    if ends.peek().is_none() {
        // END missing: a complete JSON object after the last BEGIN is still
        // unambiguous (the QR code after it is ignored).
        let start = text.rfind(BEGIN).unwrap_or(0) + BEGIN.len();
        return match first_json_object(&text[start..]) {
            Some(v) => from_json(&v),
            None => Err(KeyTextError::NotAKey(format!(
                "the paper key is incomplete: the line {END} is missing and the key text \
                 after {BEGIN} is cut off. Use the complete file or print the paper key again."
            ))),
        };
    }
    for end in ends {
        let Some(begin) = text[..end].rfind(BEGIN) else {
            continue;
        };
        let inner = &text[begin + BEGIN.len()..end];
        match first_json_object(inner) {
            Some(v) => match from_json(&v) {
                Err(e @ KeyTextError::Protected(_)) => return Err(e),
                Err(e) => {
                    first_error.get_or_insert(e);
                }
                ok => return ok,
            },
            None => {
                first_error.get_or_insert(KeyTextError::NotAKey(format!(
                    "the text between {BEGIN} and {END} is not a key"
                )));
            }
        }
    }
    Err(first_error
        .unwrap_or_else(|| KeyTextError::NotAKey(format!("no {BEGIN} line found before {END}"))))
}

fn from_json(v: &Value) -> Result<Key, KeyTextError> {
    let obj = v
        .as_object()
        .ok_or_else(|| KeyTextError::NotAKey("not a key (JSON object expected)".into()))?;
    match obj.get("kdf") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if s.eq_ignore_ascii_case("none") => {}
        Some(other) => {
            let name = match other {
                Value::String(s) => s.clone(),
                Value::Object(o) => o.keys().next().cloned().unwrap_or_default(),
                _ => other.to_string(),
            };
            return Err(KeyTextError::Protected(name.to_lowercase()));
        }
    }
    let data = obj
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| KeyTextError::NotAKey("the key has no \"data\" value".into()))?;
    let data = normalize_data(data)?;
    let text = |k: &str| obj.get(k).and_then(Value::as_str).map(String::from);
    let mut extra = obj.clone();
    for k in ["kdf", "created", "modified", "data", "fingerprint"] {
        extra.remove(k);
    }
    Ok(build(
        text("created"),
        text("modified"),
        &data,
        text("fingerprint").filter(|f| !f.is_empty()),
    )
    .with_extra(extra))
}

fn build(created: Option<String>, modified: Option<String>, data: &str, fp: Option<String>) -> Key {
    let mut m = Map::new();
    m.insert("kdf".into(), Value::Null);
    m.insert(
        "created".into(),
        Value::String(created.unwrap_or_else(|| FALLBACK_TIME.into())),
    );
    m.insert(
        "modified".into(),
        Value::String(modified.unwrap_or_else(|| FALLBACK_TIME.into())),
    );
    m.insert("data".into(), Value::String(data.into()));
    if let Some(fp) = &fp {
        m.insert("fingerprint".into(), Value::String(fp.clone()));
    }
    Key {
        json: Value::Object(m).to_string(),
        fingerprint: fp,
    }
}

impl Key {
    /// Keep fields we do not interpret (e.g. `hint`), as the client wrote them.
    fn with_extra(mut self, extra: Map<String, Value>) -> Key {
        if extra.is_empty() {
            return self;
        }
        if let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(&self.json) {
            m.extend(extra);
            self.json = Value::Object(m).to_string();
        }
        self
    }
}

/// The first complete JSON value that starts at the first `{` of `text`
/// (anything after it, e.g. a QR code, is ignored).
fn first_json_object(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    serde_json::Deserializer::from_str(&text[start..])
        .into_iter::<Value>()
        .next()?
        .ok()
}

fn looks_like_html(text: &str) -> bool {
    let head: String = text.chars().take(2048).collect::<String>().to_lowercase();
    head.contains("<html") || head.contains("<!doctype html") || head.contains("<p>")
}

/// Drop tags (including the `<img>` with the QR code) and decode the few
/// entities an html writer may use.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push('\n');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x22;", "\"")
        .replace("&#43;", "+")
        .replace("&#x2b;", "+")
        .replace("&#47;", "/")
        .replace("&#x2f;", "/")
        .replace("&#61;", "=")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    let trimmed = s.trim_end_matches('=');
    if s.len() - trimmed.len() > 2 {
        return None;
    }
    for c in trimmed.chars() {
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA: &str = "RVhBTVBMRS1LRVktTk9ULUEtUkVBTC1LRVktMDEyMzQ=";
    const FP: &str = "3e:07:87:f4:da:6e:70:47";

    fn block() -> String {
        format!(
            "{BEGIN}\n{{\n  \"kdf\": null,\n  \"created\": \"2026-11-09T20:54:47+00:00\",\n  \
             \"modified\": \"2026-11-09T20:54:47+00:00\",\n  \"data\": \"{DATA}\",\n  \
             \"fingerprint\": \"{FP}\"\n}}\n{END}\n"
        )
    }

    fn data_of(k: &Key) -> String {
        let v: Value = serde_json::from_str(&k.json).unwrap();
        v["data"].as_str().unwrap().to_string()
    }

    #[test]
    fn text_with_header_and_qr_code() {
        let text = format!(
            "nestegg paper key for host example\nKeep it safe.\n\n{}█▀▀▀▀▀█ ▄▄ █▀▀▀▀▀█\n█ ███ █ █▀ █ ███ █\n",
            block()
        );
        let k = from_file_text(&text).unwrap();
        assert_eq!(data_of(&k), DATA);
        assert_eq!(k.fingerprint.as_deref(), Some(FP));
        let v: Value = serde_json::from_str(&k.json).unwrap();
        assert!(v["kdf"].is_null());
    }

    #[test]
    fn header_that_mentions_the_markers() {
        let text = format!(
            "Copy the lines between {BEGIN} and {END} when you type it.\n{}",
            block()
        );
        assert_eq!(data_of(&from_file_text(&text).unwrap()), DATA);
        let only_begin = format!("Starts with {BEGIN} like this:\n{}", block());
        assert_eq!(data_of(&from_file_text(&only_begin).unwrap()), DATA);
    }

    #[test]
    fn html_with_image() {
        let html = format!(
            "<!DOCTYPE html>\n<html><head><title>Proxmox Backup Paperkey</title></head><body>\n\
             <div><p>\n{}</p>\n<center>\n<img\nwidth=\"500pt\" src=\"data:image/svg+xml;base64,PD94bWwgdmVyc2lvbj0i\">\n</center></div></body></html>\n",
            block()
        );
        let k = from_file_text(&html).unwrap();
        assert_eq!(data_of(&k), DATA);
        // Entities as a different writer might emit them.
        let escaped = html.replace('"', "&quot;");
        assert_eq!(data_of(&from_file_text(&escaped).unwrap()), DATA);
    }

    #[test]
    fn pasted_with_crlf_and_stray_spaces() {
        let text = block().replace('\n', "\r\n").replace("  \"", "      \t \"");
        let k = from_file_text(&format!("   {text}   ")).unwrap();
        assert_eq!(data_of(&k), DATA);
        let k = from_typed(&text).unwrap();
        assert_eq!(data_of(&k), DATA);
    }

    #[test]
    fn missing_end_marker() {
        // Complete key, QR code and end line lost: still unambiguous.
        let cut = block().replace(END, "");
        assert_eq!(data_of(&from_file_text(&cut).unwrap()), DATA);
        // Key text itself cut off: refuse and name the marker.
        let truncated = &block()[..block().find("\"data\"").unwrap() + 20];
        let e = from_file_text(truncated).unwrap_err().to_string();
        assert!(
            e.contains("incomplete") && e.contains("END PROXMOX BACKUP KEY"),
            "{e}"
        );
    }

    #[test]
    fn garbage_is_not_a_key() {
        for text in [
            "",
            "hello world",
            "-----BEGIN PROXMOX BACKUP KEY-----\nnot json\n-----END PROXMOX BACKUP KEY-----",
            "-----BEGIN PROXMOX BACKUP KEY-----\n{\"a\":1}\n-----END PROXMOX BACKUP KEY-----",
            "{\"kdf\":null}",
            "[1,2,3]",
            "-----END PROXMOX BACKUP KEY-----",
            "<html><body>nothing here</body></html>",
        ] {
            assert!(
                matches!(from_file_text(text), Err(KeyTextError::NotAKey(_))),
                "{text:?}"
            );
        }
    }

    #[test]
    fn exported_key_file_is_accepted() {
        let json = format!(
            r#"{{"kdf":null,"created":"2026-01-01T00:00:00+00:00","modified":"2026-01-01T00:00:00+00:00","data":"{DATA}","fingerprint":"{FP}"}}"#
        );
        let k = from_file_text(&json).unwrap();
        assert_eq!(data_of(&k), DATA);
        assert_eq!(k.fingerprint.as_deref(), Some(FP));
        // `kdf: "none"` as printed by `key show`.
        assert!(from_file_text(&json.replace("null", "\"none\"")).is_ok());
    }

    #[test]
    fn password_protected_key_is_named() {
        let scrypt = format!(
            r#"{{"kdf":{{"Scrypt":{{"n":65536,"r":8,"p":1,"salt":"AAAA"}}}},"created":"x","modified":"x","data":"{DATA}"}}"#
        );
        let e = from_file_text(&scrypt).unwrap_err();
        assert!(
            matches!(&e, KeyTextError::Protected(k) if k == "scrypt"),
            "{e}"
        );
        assert!(e.to_string().contains("protected with a password"));
        let paper = format!("{BEGIN}\n{scrypt}\n{END}\n");
        assert!(matches!(
            from_file_text(&paper),
            Err(KeyTextError::Protected(_))
        ));
    }

    #[test]
    fn typed_data_line_in_many_forms() {
        for input in [
            DATA.to_string(),
            format!("  {DATA}  "),
            format!("\"{DATA}\""),
            format!("\"{DATA}\","),
            format!("  \"data\": \"{DATA}\",  "),
            format!("'{DATA}'"),
            // blanks inside, as pasted from a wrapped printout
            format!("{} {} {}", &DATA[..15], &DATA[15..30], &DATA[30..]),
            // final = forgotten
            DATA.trim_end_matches('=').to_string(),
        ] {
            let k = from_typed(&input).unwrap_or_else(|e| panic!("{input:?}: {e}"));
            assert_eq!(data_of(&k), DATA, "{input:?}");
            assert!(k.fingerprint.is_none());
        }
        // The client accepts exactly this file (checked with the real client).
        let k = from_typed(DATA).unwrap();
        let v: Value = serde_json::from_str(&k.json).unwrap();
        assert!(v["created"].is_string() && v["modified"].is_string());
    }

    #[test]
    fn typed_errors_are_specific() {
        let e = from_typed(&DATA[..30]).unwrap_err().to_string();
        assert!(e.contains("got 30 characters") && e.contains("44"), "{e}");
        let e = from_typed(&DATA.replace('+', "_")).unwrap_err().to_string();
        assert!(
            e.contains("character 31") && e.contains("keyboard layout"),
            "{e}"
        );
        let e = from_typed(&format!("{DATA}x")).unwrap_err().to_string();
        assert!(
            e.contains("45 characters") || e.contains("cannot be part"),
            "{e}"
        );
        // `=` only at the end
        assert!(from_typed(&DATA.replace('A', "=")).is_err());
        // right length, wrong padding: does not decode to 32 bytes
        assert!(from_typed(&format!("{}==", &DATA[..42])).is_err());
        assert!(from_typed("").is_err());
    }

    #[test]
    fn typed_whole_json_is_accepted() {
        assert_eq!(data_of(&from_typed(&block()).unwrap()), DATA);
        let one_line = format!(r#"{{"kdf":null,"created":"a","modified":"b","data":"{DATA}"}}"#);
        assert_eq!(data_of(&from_typed(&one_line).unwrap()), DATA);
    }

    #[test]
    fn unknown_fields_survive() {
        let json =
            format!(r#"{{"kdf":null,"created":"a","modified":"b","data":"{DATA}","hint":"blue"}}"#);
        let k = from_file_text(&json).unwrap();
        let v: Value = serde_json::from_str(&k.json).unwrap();
        assert_eq!(v["hint"], "blue");
    }

    #[test]
    fn base64_decoder() {
        assert_eq!(decode_base64("TWFu").unwrap(), b"Man");
        assert_eq!(decode_base64("TWE=").unwrap(), b"Ma");
        assert_eq!(decode_base64(DATA).unwrap().len(), 32);
        assert!(decode_base64("T*Fu").is_none());
    }
}
