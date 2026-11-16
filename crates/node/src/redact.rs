// Vendored from Nightgrid (https://github.com/TheRoben-Dev/nightgrid),
// core/src/redact.rs at commit 8eedcf2, MIT License, (c) 2026 TheRoben.
// Unchanged, so the node drops the same lines the app would.

//! Spotting text that looks like it carries a secret.
//!
//! Used where user text would otherwise be stored or shown (notes, shell
//! history search). It flags a subset of what the UI's `redact()` masks
//! (`ui/src/lib/helper/prompt.ts`), so text masked there always passes here.

/// What kind of secret `text` seems to hold, or `None`.
pub fn secret_hint(text: &str) -> Option<&'static str> {
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        return Some("a private key");
    }
    text.lines().find_map(line_hint)
}

/// Whether one line (a shell command, say) looks like it carries a secret.
pub fn line_has_secret(line: &str) -> bool {
    line.contains("PRIVATE KEY-----") || line_hint(line).is_some()
}

fn line_hint(line: &str) -> Option<&'static str> {
    let lower = line.to_ascii_lowercase();
    if url_password(&lower) {
        return Some("a password in a URL");
    }
    if bearer(&lower) {
        return Some("a bearer token");
    }
    if assignment(line, &lower) {
        return Some("a password or token");
    }
    None
}

const KEYWORDS: &[&str] = &[
    "password",
    "passwd",
    "pass",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "accesskey",
    "access_key",
    "access-key",
    "private_key",
    "privatekey",
    "private-key",
    "credential",
    "pwd",
];

fn ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-')
}

/// `password=hunter2`, `API_TOKEN: abc…`, `--password "x y"` is not matched
/// (no `=`/`:`), which keeps ordinary prose like "token: 5" out by length.
fn assignment(line: &str, lower: &str) -> bool {
    let b = lower.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !ident_char(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && ident_char(b[i]) {
            i += 1;
        }
        let ident = &lower[start..i];
        if !KEYWORDS.iter().any(|k| ident.contains(k)) {
            continue;
        }
        let mut j = i;
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        if j >= b.len() || !(b[j] == b'=' || b[j] == b':') {
            continue;
        }
        j += 1;
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        let rest = &line[j..];
        let value = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or(""),
            _ => rest.split(|c: char| c.is_whitespace() || c == ',' || c == ';').next().unwrap_or(""),
        };
        if real_value(value) {
            return true;
        }
    }
    false
}

/// A value long enough to matter that isn't a placeholder or a reference.
fn real_value(v: &str) -> bool {
    let v = v.trim();
    if v.chars().count() < 4 || v.contains('•') {
        return false;
    }
    if v.starts_with(['$', '<', '{', '%', '[']) || v.starts_with("${") {
        return false;
    }
    !v.chars().all(|c| matches!(c, '*' | 'x' | 'X' | '.' | '-'))
}

/// `scheme://user:pass@host`.
fn url_password(lower: &str) -> bool {
    let mut from = 0;
    while let Some(p) = lower[from..].find("://") {
        let after = &lower[from + p + 3..];
        let auth: &str = after.split(['/', ' ', '\t']).next().unwrap_or("");
        if let Some(at) = auth.rfind('@') {
            if let Some((user, pass)) = auth[..at].split_once(':') {
                if !user.is_empty() && real_value(pass) {
                    return true;
                }
            }
        }
        from += p + 3;
    }
    false
}

/// `Bearer eyJhbGciOi…` (12+ token characters including a digit).
fn bearer(lower: &str) -> bool {
    let mut from = 0;
    while let Some(p) = lower[from..].find("bearer") {
        let rest = lower[from + p + 6..].trim_start();
        let tok: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || "._~+/=-".contains(*c)).collect();
        if tok.len() >= 12 && tok.chars().any(|c| c.is_ascii_digit()) {
            return true;
        }
        from += p + 6;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_secrets() {
        assert_eq!(secret_hint("DB_PASSWORD=hunter22"), Some("a password or token"));
        assert_eq!(secret_hint("api_key: \"abc def ghi\""), Some("a password or token"));
        assert_eq!(secret_hint("restic -r s3://me:Sup3rSecret@host/repo"), Some("a password in a URL"));
        assert_eq!(secret_hint("curl -H 'Authorization: Bearer abcdef123456789'"), Some("a bearer token"));
        assert_eq!(secret_hint("x\n-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n"), Some("a private key"));
        assert!(line_has_secret("export GITHUB_TOKEN=ghp_0123456789abcdef"));
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        for ok in [
            "Restart the stack with `docker compose up -d`",
            "token: 5",
            "password=••••",
            "PASSWORD=${DB_PASSWORD}",
            "password: <your password>",
            "API_KEY=$API_KEY",
            "secret=****",
            "ssh admin@nas.lan",
            "https://example.com/path",
            "Bearer tokens are sent in a header",
            "passwd alice",
        ] {
            assert_eq!(secret_hint(ok), None, "{ok}");
        }
    }
}
