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
//! A damaged `fingerprint` makes the client refuse the whole key
//! ("KeyConfig contains wrong fingerprint", "fingerprint too short"), so it
//! is never copied into the key file built here: the client computes it.
//! A `data` value whose last character carries non-zero padding bits is
//! refused by the client ("error decoding base64 data"), so it is refused
//! here as well.

use serde_json::{Map, Value};

pub const BEGIN: &str = "-----BEGIN PROXMOX BACKUP KEY-----";
pub const END: &str = "-----END PROXMOX BACKUP KEY-----";

/// Length of the `data` value: 32 bytes in base64, with padding.
pub const DATA_CHARS: usize = 44;
/// Used when a pasted key has no (usable) time stamps; the client only
/// needs the fields to exist.
const FALLBACK_TIME: &str = "1970-01-01T00:00:00+00:00";

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
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
    /// Key file content (one line of JSON, without `fingerprint`).
    pub json: String,
    /// Said when the text held more than one key block: how many, and
    /// which one is used.
    pub note: Option<String>,
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
    Ok(build(None, None, &data))
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
        _ => {
            // 43 characters carry 258 bits, the key has 256: the last
            // character before `=` must leave the 2 extra bits zero.
            let last = s.chars().nth(DATA_CHARS - 2).unwrap_or('?');
            if s.ends_with('=')
                && !s.ends_with("==")
                && base64_value(last).is_some_and(|v| v % 4 != 0)
            {
                return Err(KeyTextError::NotAKey(format!(
                    "character {} ('{last}') cannot be the last character before the =: the \
                     data line of a key ends with one of A E I M Q U Y c g k o s w 0 4 8 and \
                     then =. Check the last characters.",
                    DATA_CHARS - 1
                )));
            }
            Err(KeyTextError::NotAKey(
                "this is not a valid key: the characters do not decode to 32 bytes".into(),
            ))
        }
    }
}

fn from_block(text: &str) -> Result<Key, KeyTextError> {
    // Each END marker closes the text since the last BEGIN before it that
    // no earlier END has closed. One pass over the marker positions, each
    // block parsed once: linear in the size of the text, however many
    // markers a (crafted) file contains.
    let mut markers: Vec<(usize, bool)> = text
        .match_indices(BEGIN)
        .map(|(i, _)| (i, true))
        .chain(text.match_indices(END).map(|(i, _)| (i, false)))
        .collect();
    markers.sort_unstable();
    if !markers.iter().any(|(_, begin)| !begin) {
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
    let mut open = None;
    // Blocks that hold JSON (a header line that names both markers does
    // not count as a block), and the first error of the others.
    let mut results = Vec::new();
    let mut no_json = None;
    for (at, begin) in markers {
        if begin {
            open = Some(at + BEGIN.len());
        } else if let Some(start) = open.take() {
            match first_json_object(&text[start..at]) {
                Some(v) => results.push(from_json(&v)),
                None => {
                    no_json.get_or_insert(KeyTextError::NotAKey(format!(
                        "the text between {BEGIN} and {END} is not a key"
                    )));
                }
            }
        }
    }
    if let Some(e) = results.iter().find_map(|r| {
        r.as_ref()
            .err()
            .filter(|e| matches!(e, KeyTextError::Protected(_)))
    }) {
        return Err(e.clone());
    }
    let Some(used) = results.iter().position(Result::is_ok) else {
        return Err(results
            .into_iter()
            .find_map(Result::err)
            .or(no_json)
            .unwrap_or_else(|| {
                KeyTextError::NotAKey(format!("no {BEGIN} line found before {END}"))
            }));
    };
    let mut key = results[used].clone().expect("checked above");
    if results.len() > 1 {
        let mut distinct: Vec<String> = results
            .iter()
            .flatten()
            .map(|k| data_of_json(&k.json))
            .collect();
        distinct.sort();
        distinct.dedup();
        key.note = Some(format!(
            "this file contains {} key blocks ({} different valid key{}); block {} is used, \
             the first one that is a valid key.{}",
            results.len(),
            distinct.len(),
            if distinct.len() == 1 { "" } else { "s" },
            used + 1,
            if distinct.len() > 1 {
                " If it does not fit the backup, save the other block as a file of its own."
            } else {
                ""
            }
        ));
    }
    Ok(key)
}

fn data_of_json(json: &str) -> String {
    serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|v| v["data"].as_str().map(String::from))
        .unwrap_or_default()
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
    Ok(build(text("created"), text("modified"), &data).with_extra(extra))
}

/// The key file for the client. `fingerprint` is left out on purpose: the
/// client computes it from `data`, and a damaged copy of it (it is not
/// needed to restore) would make the client refuse the key.
fn build(created: Option<String>, modified: Option<String>, data: &str) -> Key {
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
    Key {
        json: Value::Object(m).to_string(),
        note: None,
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

/// Drop tags (including the `<img>` with the QR code) and decode entities.
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
    decode_entities(&out)
}

/// Decode `&#NN;`, `&#xHH;` (either case) and the named entities an html
/// writer may use for the characters of a key, in one pass (so `&amp;quot;`
/// stays `&quot;`). Unknown entities are kept as they are.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let decoded = rest[1..]
            .find(';')
            .filter(|&end| end <= 10)
            .and_then(|end| Some((entity(&rest[1..1 + end])?, end + 2)));
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match name.to_ascii_lowercase().as_str() {
        "quot" => '"',
        "apos" => '\'',
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "nbsp" => ' ',
        "plus" => '+',
        "sol" => '/',
        "equals" => '=',
        "colon" => ':',
        "comma" => ',',
        "lbrace" | "lcub" => '{',
        "rbrace" | "rcub" => '}',
        _ => return None,
    })
}

fn base64_value(c: char) -> Option<u32> {
    Some(match c {
        'A'..='Z' => c as u32 - 'A' as u32,
        'a'..='z' => c as u32 - 'a' as u32 + 26,
        '0'..='9' => c as u32 - '0' as u32 + 52,
        '+' => 62,
        '/' => 63,
        _ => return None,
    })
}

/// Strict base64 (standard alphabet, padded): the length is a multiple of
/// four, and the bits after the last full byte are zero, as the decoder of
/// the client demands.
fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    let trimmed = s.trim_end_matches('=');
    if s.len() - trimmed.len() > 2 || s.len() % 4 != 0 {
        return None;
    }
    for c in trimmed.chars() {
        let v = base64_value(c)?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (acc == 0).then_some(out)
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
        let v: Value = serde_json::from_str(&k.json).unwrap();
        assert!(v["kdf"].is_null());
        // The client computes the fingerprint; a copy is never passed on.
        assert!(v.get("fingerprint").is_none(), "{v}");
        assert_eq!(k.note, None);
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
        assert!(!k.json.contains("fingerprint"));
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
        let mut bad = DATA.to_string();
        bad.replace_range(30..31, "_");
        let e = from_typed(&bad).unwrap_err().to_string();
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
        assert!(from_typed(&format!("{}={}", &DATA[..10], &DATA[11..])).is_err());
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
        // Padding bits must be zero ("TWE=" = "Ma"; "TWF=" has a stray bit).
        assert!(decode_base64("TWF=").is_none());
        assert!(decode_base64("TWFu=").is_none());
    }

    #[test]
    fn last_character_must_leave_the_padding_bits_zero() {
        // 'Q' (16) is the right last character; the other three values
        // that share its upper four bits (R, S, T) were accepted before
        // and refused by the client ("error decoding base64 data").
        assert_eq!(&DATA[42..], "Q=");
        for typo in ['R', 'S', 'T', 'q', '1'] {
            let input = format!("{}{typo}=", &DATA[..42]);
            let e = from_typed(&input).unwrap_err().to_string();
            assert!(
                e.contains("character 43") && e.contains("last character"),
                "{typo}: {e}"
            );
        }
        let damaged = format!(
            r#"{{"kdf":null,"created":"a","modified":"b","data":"{}R="}}"#,
            &DATA[..42]
        );
        assert!(from_file_text(&damaged).is_err());
    }

    #[test]
    fn damaged_fingerprint_does_not_spoil_the_key() {
        // The real client refuses a key file whose fingerprint is wrong
        // ("KeyConfig contains wrong fingerprint") or malformed
        // ("fingerprint too short"), although it is not needed.
        for fp in ["00:11:22:33", "garbage", ""] {
            let json = format!(
                r#"{{"kdf":null,"created":"a","modified":"b","data":"{DATA}","fingerprint":"{fp}"}}"#
            );
            let k = from_file_text(&json).unwrap();
            assert!(!k.json.contains("fingerprint"), "{}", k.json);
            assert_eq!(data_of(&k), DATA);
        }
    }

    #[test]
    fn html_entities_in_any_case() {
        let encoded = DATA
            .replace('+', "&#x2B;")
            .replace('/', "&#X2F;")
            .replace('=', "&#x3D;");
        for (quote, data) in [
            ("&#x22;", encoded.clone()),
            ("&#X22;", DATA.replace('=', "&#61;")),
            ("&QUOT;", DATA.to_string()),
            ("&#34;", DATA.replace('=', "&#x3d;")),
        ] {
            let html = format!(
                "<html><body><p>{BEGIN}\n{{{quote}kdf{quote}: null, {quote}created{quote}: \
                 {quote}a{quote}, {quote}modified{quote}: {quote}b{quote}, {quote}data{quote}: \
                 {quote}{data}{quote}}}\n{END}</p></body></html>"
            );
            let k = from_file_text(&html).unwrap_or_else(|e| panic!("{quote}: {e}"));
            assert_eq!(data_of(&k), DATA, "{quote}");
        }
        assert_eq!(
            decode_entities("&amp;quot; &unknown; & &#xZZ;"),
            "&quot; &unknown; & &#xZZ;"
        );
    }

    #[test]
    fn several_key_blocks_are_named() {
        let other = "T1RIRVItRVhBTVBMRS1LRVktTk9ULVJFQUwtMDAwMDA=";
        let second = block().replace(DATA, other);
        let k = from_file_text(&format!("{}{second}", block())).unwrap();
        assert_eq!(data_of(&k), DATA);
        let note = k.note.unwrap();
        assert!(
            note.contains("2 key blocks")
                && note.contains("2 different")
                && note.contains("block 1"),
            "{note}"
        );
        // A broken first block: the second one is used, and that is said.
        let broken = block().replace(DATA, "not-a-key");
        let k = from_file_text(&format!("{broken}{second}")).unwrap();
        assert_eq!(data_of(&k), other);
        assert!(k.note.unwrap().contains("block 2"));
        // The same key twice.
        let k = from_file_text(&format!("{}{}", block(), block())).unwrap();
        assert!(k.note.unwrap().contains("1 different valid key"));
        // A header that names both markers is not a block.
        let text = format!("Copy what is between {BEGIN} and {END}.\n{}", block());
        assert_eq!(from_file_text(&text).unwrap().note, None);
    }

    #[test]
    fn many_markers_are_parsed_in_linear_time() {
        // About 1 MB of END lines (and of BEGIN/END pairs) used to cost
        // seconds; it must stay well below that.
        for crafted in [
            format!("{BEGIN}\n{}", format!("{END}\n").repeat(30_000)),
            format!("{BEGIN}\n{{\n{END}\n").repeat(15_000),
        ] {
            let start = std::time::Instant::now();
            assert!(from_file_text(&crafted).is_err());
            assert!(
                start.elapsed() < std::time::Duration::from_millis(500),
                "{:?} for {} bytes",
                start.elapsed(),
                crafted.len()
            );
        }
    }

    /// Paper keys as `nestegg key paperkey` writes them: the output of the
    /// real client (`proxmox-backup-client key paperkey`, 4.2.8, for the
    /// example key) with nestegg's header in front. Both readers of saved
    /// paper keys must accept them: this one (rescue stick) and the one of
    /// `nestegg key confirm-exported --file` (`keyproof`).
    #[test]
    fn real_paper_key_files_are_read_by_both_parsers() {
        use crate::keyproof;
        let header = keyproof::PaperHeader {
            backup_id: "desktop".into(),
            hostname: "desktop".into(),
            date: "2026-11-09".into(),
            server: "pbs.example.com".into(),
            datastore: "datastore".into(),
            fingerprint: "<FINGERPRINT>".into(),
            check_code: keyproof::check_code(DATA),
        };
        let text = include_bytes!("../../testdata/paperkey/client-paperkey.txt");
        let html = include_bytes!("../../testdata/paperkey/client-paperkey.html");
        let files = [
            ("text", keyproof::compose_text(&header, text)),
            ("html", keyproof::compose_html(&header, html).unwrap()),
            ("client text", text.to_vec()),
            ("client html", html.to_vec()),
        ];
        for (name, bytes) in files {
            let k = from_file_text(&String::from_utf8_lossy(&bytes))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(data_of(&k), DATA, "{name}");
            assert_eq!(k.note, None, "{name}");
            let material = keyproof::KeyMaterial::from_copy(&bytes)
                .unwrap_or_else(|e| panic!("keyproof, {name}: {e:#}"));
            assert_eq!(material.check_code(), keyproof::check_code(DATA), "{name}");
        }
    }
}
