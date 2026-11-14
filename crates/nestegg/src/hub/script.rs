use anyhow::Result;

pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn render(url: &str, pin: &str) -> Result<String> {
    Ok(include_str!("join.sh")
        .replace("@HUB_URL@", &quote(url.trim_end_matches('/')))
        .replace("@HUB_PIN@", &quote(pin)))
}

pub fn command(url: &str, pin: &str, code: &str) -> String {
    format!(
        "curl -fsSk --pinnedpubkey {} {} | sudo sh -s -- {}",
        quote(pin),
        quote(&format!("{}/join.sh", url.trim_end_matches('/'))),
        quote(code)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hub_downloads_are_pinned_and_script_is_posix() {
        let script = render("https://hub.example.com:8443", "sha256//test").unwrap();
        assert!(!script.contains("@HUB_"));
        assert!(script.contains("-k --pinnedpubkey \"$pin\""));
        assert!(script.contains("nestegg join \"$hub\" --pin \"$pin\" --code \"$code\""));
        assert!(
            script.contains("136673be77aba35dcce385b28737689ad64fd785a797e57897589aed08db6e45")
        );
        let mut child = std::process::Command::new("sh")
            .arg("-n")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
    }
    #[test]
    fn shell_quote_cannot_inject() {
        assert_eq!(quote("a'b"), "'a'\\''b'");
    }
}
