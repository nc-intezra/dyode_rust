//! Bearer tokens for the local API, one file per box (`/etc/dyode/api-tokens`
//! by default).  Only a SHA-256 of each token is stored; the token itself
//! is printed once, by `dyode api-token add`.
//!
//! ```text
//! # name        scopes        sha256 of the token
//! grafana       read          9c1f...e04a
//! ```
//!
//! The file is never part of config.yaml and is never copied to the other
//! box: a token from the IT side means nothing on the OT side.

use sha2::{Digest, Sha256};
use std::path::Path;

/// Scopes this version understands.
pub const SCOPES: [&str; 1] = ["read"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub name: String,
    pub scopes: Vec<String>,
    hash: [u8; 32],
}

impl Token {
    pub fn allows(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// The tokens in the file; a missing file is an empty list.
pub fn list(path: &Path) -> Result<Vec<Token>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        let bad = || {
            format!(
                "{}:{}: expected 'name scopes sha256'",
                path.display(),
                n + 1
            )
        };
        if f.len() != 3 {
            return Err(bad());
        }
        let hash = parse_hex32(f[2]).ok_or_else(bad)?;
        out.push(Token {
            name: f[0].to_string(),
            scopes: f[1].split(',').map(String::from).collect(),
            hash,
        });
    }
    Ok(out)
}

/// The token's entry, compared in constant time against every entry.
pub fn verify(tokens: &[Token], presented: &str) -> Option<Token> {
    let d = digest(presented);
    let mut found = None;
    for t in tokens {
        let diff = t
            .hash
            .iter()
            .zip(d.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b));
        if diff == 0 && found.is_none() {
            found = Some(t.clone());
        }
    }
    found
}

fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        return Err("a token name is 1 to 64 letters, digits, '-', '_' or '.'".into());
    }
    Ok(())
}

fn write(path: &Path, lines: &[String]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    let mut text =
        String::from("# DYODE API tokens (dyode api-token): name, scopes, SHA-256 of the token.\n");
    for l in lines {
        text.push_str(l);
        text.push('\n');
    }
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn line(t: &Token) -> String {
    format!(
        "{:<20} {:<12} {}",
        t.name,
        t.scopes.join(","),
        crate::hex::encode(t.hash)
    )
}

/// Add a token; returns the token itself, which is not stored.
pub fn add(path: &Path, name: &str, scopes: &[String]) -> Result<String, String> {
    check_name(name)?;
    if scopes.is_empty() {
        return Err("give at least one scope".into());
    }
    for s in scopes {
        if !SCOPES.contains(&s.as_str()) {
            return Err(format!(
                "unknown scope {s:?} (known: {})",
                SCOPES.join(", ")
            ));
        }
    }
    let mut tokens = list(path)?;
    if tokens.iter().any(|t| t.name == name) {
        return Err(format!("a token named {name:?} exists; revoke it first"));
    }
    let mut raw = [0u8; 32];
    crate::auth::random_bytes(&mut raw);
    let token = format!("dyode_{}", crate::hex::encode(raw));
    tokens.push(Token {
        name: name.into(),
        scopes: scopes.to_vec(),
        hash: digest(&token),
    });
    write(path, &tokens.iter().map(line).collect::<Vec<_>>())?;
    Ok(token)
}

/// Remove a token by name.  False when there was none.
pub fn revoke(path: &Path, name: &str) -> Result<bool, String> {
    let tokens = list(path)?;
    let kept: Vec<Token> = tokens.iter().filter(|t| t.name != name).cloned().collect();
    if kept.len() == tokens.len() {
        return Ok(false);
    }
    write(path, &kept.iter().map(line).collect::<Vec<_>>())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_verify_revoke() {
        let d = crate::testutil::TempDir::new();
        let p = d.path().join("api-tokens");
        assert!(list(&p).unwrap().is_empty());
        let t = add(&p, "grafana", &["read".into()]).unwrap();
        assert!(t.starts_with("dyode_") && t.len() == 70);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains(&t), "the token itself must not be stored");
        let all = list(&p).unwrap();
        assert_eq!(verify(&all, &t).unwrap().name, "grafana");
        assert!(verify(&all, "dyode_wrong").is_none());
        assert!(add(&p, "grafana", &["read".into()]).is_err());
        assert!(add(&p, "x y", &["read".into()]).is_err());
        assert!(add(&p, "z", &["write".into()]).is_err());
        assert!(revoke(&p, "grafana").unwrap());
        assert!(!revoke(&p, "grafana").unwrap());
        assert!(verify(&list(&p).unwrap(), &t).is_none());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
