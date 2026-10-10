//! Removes every cluster secret's plaintext from text that will be shown to
//! an AI client.
//!
//! The AI sees container logs, stack files, config and the like, any of which
//! can echo a secret: raw, inside an HTTP basic-auth header (base64 of
//! `user:secret`), in a URL (percent-encoded), in a JSON log line (escaped).
//! So for each secret we match not just the value but its common encodings,
//! including encodings of the value embedded at any alignment inside a larger
//! blob.
//!
//! Nothing here is ever allowed to print a value: `Debug` shows names and
//! counts only. The automaton itself necessarily contains the needles (which
//! are secret-equivalent) and cannot be zeroized; it lives only in memory and
//! is dropped with the `Redactor`.

use std::collections::HashSet;
use std::fmt;

use aho_corasick::{AhoCorasick, MatchKind};
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::Engine;
use cl_raft::ClusterState;
use cl_secret::{ClusterKey, Sealed};
use zeroize::Zeroizing;

/// Needles shorter than this would match all over ordinary text and wreck the
/// output, so a secret that short cannot be redacted (and is reported).
const MIN_NEEDLE_LEN: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum RedactError {
    #[error("failed to build the redaction matcher: {0}")]
    Build(String),
}

pub struct Redactor {
    matcher: Option<AhoCorasick>,
    /// Pattern index -> secret name, parallel to the matcher's patterns.
    names: Vec<String>,
    /// Secrets that could not be decrypted (wrong key, corrupt). Their values
    /// may leak, so callers should warn.
    pub unredactable: Vec<String>,
    /// Secrets too short to be matched safely.
    pub too_short: Vec<String>,
}

impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redactor")
            .field("needles", &self.names.len())
            .field("unredactable", &self.unredactable)
            .field("too_short", &self.too_short)
            .finish()
    }
}

impl Redactor {
    /// Decrypt every secret, internal ones included: an AI must not see the
    /// cluster's own credentials any more than a user's.
    pub fn from_state(state: &ClusterState, key: &ClusterKey) -> Result<Redactor, RedactError> {
        let fingerprint = key.fingerprint();
        let mut plaintexts = Vec::new();
        let mut unredactable = Vec::new();
        for (name, entry) in &state.secrets {
            if entry.key_fingerprint != fingerprint {
                unredactable.push(name.clone());
                continue;
            }
            match Sealed::from_blob(entry.sealed.clone()).open(key, name) {
                Ok(p) => plaintexts.push((name.clone(), p)),
                Err(_) => unredactable.push(name.clone()),
            }
        }
        let mut redactor = Self::from_plaintexts(plaintexts)?;
        redactor.unredactable = unredactable;
        Ok(redactor)
    }

    pub fn from_plaintexts(secrets: Vec<(String, Vec<u8>)>) -> Result<Redactor, RedactError> {
        let secrets: Vec<(String, Zeroizing<Vec<u8>>)> = secrets
            .into_iter()
            .map(|(n, v)| (n, Zeroizing::new(v)))
            .collect();

        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        let mut patterns: Vec<Zeroizing<Vec<u8>>> = Vec::new();
        let mut names = Vec::new();
        let mut too_short = Vec::new();

        for (name, value) in &secrets {
            if value.is_empty() {
                continue;
            }
            if trim_ws(value).len() < MIN_NEEDLE_LEN {
                too_short.push(name.clone());
            }
            for needle in needles_for(value) {
                // Same bytes under two secrets: the first name wins.
                if !seen.insert(needle.to_vec()) {
                    continue;
                }
                names.push(name.clone());
                patterns.push(needle);
            }
        }
        // The set holds secret-equivalent copies; scrub them too.
        for mut k in seen {
            zeroize::Zeroize::zeroize(&mut k);
        }

        let matcher = if patterns.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::builder()
                    .match_kind(MatchKind::LeftmostLongest)
                    .build(patterns.iter().map(|p| p.as_slice()))
                    .map_err(|e| RedactError::Build(e.to_string()))?,
            )
        };
        Ok(Redactor {
            matcher,
            names,
            unredactable: Vec::new(),
            too_short,
        })
    }

    pub fn redact(&self, text: &str) -> String {
        String::from_utf8_lossy(&self.redact_bytes(text.as_bytes())).into_owned()
    }

    pub fn redact_bytes(&self, data: &[u8]) -> Vec<u8> {
        let Some(matcher) = &self.matcher else {
            return data.to_vec();
        };
        let mut out = Vec::with_capacity(data.len());
        let mut last = 0;
        for m in matcher.find_iter(data) {
            out.extend_from_slice(&data[last..m.start()]);
            out.extend_from_slice(b"[REDACTED secret:");
            out.extend_from_slice(self.names[m.pattern().as_usize()].as_bytes());
            out.push(b']');
            last = m.end();
        }
        out.extend_from_slice(&data[last..]);
        out
    }
}

fn trim_ws(v: &[u8]) -> &[u8] {
    match v.iter().position(|b| !b.is_ascii_whitespace()) {
        None => &[],
        Some(s) => {
            let e = v.iter().rposition(|b| !b.is_ascii_whitespace()).unwrap();
            &v[s..=e]
        }
    }
}

/// Every form of the value we want to catch. Candidates are the raw value, its
/// trimmed form and each non-empty trimmed line (a multi-line secret such as a
/// key file is often logged one line at a time).
fn needles_for(value: &[u8]) -> Vec<Zeroizing<Vec<u8>>> {
    let mut candidates: Vec<&[u8]> = vec![value, trim_ws(value)];
    for line in value.split(|b| *b == b'\n') {
        candidates.push(trim_ws(line));
    }
    candidates.retain(|c| c.len() >= MIN_NEEDLE_LEN);
    candidates.sort();
    candidates.dedup();

    let mut out: Vec<Zeroizing<Vec<u8>>> = Vec::new();
    let mut push = |v: Vec<u8>| {
        if v.len() >= MIN_NEEDLE_LEN {
            out.push(Zeroizing::new(v));
        }
    };
    for c in candidates {
        push(c.to_vec());
        for offset in 0..3 {
            for url in [false, true] {
                if let Some(n) = base64_middle(c, offset, url) {
                    push(n);
                }
            }
        }
        push(hex(c, false).into_bytes());
        push(hex(c, true).into_bytes());
        let pct = percent(c);
        if pct != c {
            push(pct);
        }
        let json = std::str::from_utf8(c)
            .ok()
            .and_then(|s| serde_json::to_string(s).ok());
        if let Some(j) = json {
            let inner = &j.as_bytes()[1..j.len() - 1];
            if inner != c {
                push(inner.to_vec());
            }
        }
    }
    out
}

/// The part of `base64(prefix + value + suffix)` that depends only on `value`,
/// when `prefix.len() % 3 == offset`.
///
/// Base64 maps 3 bytes to 4 chars, so where the value starts within a group
/// decides which chars it produces. Chars whose 6 bits include a byte of the
/// unknown prefix or suffix are dropped: we keep chars `ceil(8*o/6) ..
/// floor(8*(o+n)/6)`. Padding is irrelevant: `=` only follows the partial
/// last char we already excluded, so unpadded needles also match padded text.
fn base64_middle(value: &[u8], offset: usize, url: bool) -> Option<Vec<u8>> {
    let mut buf = Zeroizing::new(vec![0u8; offset]);
    buf.extend_from_slice(value);
    let enc = Zeroizing::new(if url {
        URL_SAFE_NO_PAD.encode(&*buf)
    } else {
        STANDARD_NO_PAD.encode(&*buf)
    });
    let start = (8 * offset).div_ceil(6);
    let end = (8 * (offset + value.len())) / 6;
    if end <= start {
        return None;
    }
    Some(enc.as_bytes()[start..end].to_vec())
}

fn hex(v: &[u8], upper: bool) -> String {
    let digits: &[u8; 16] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut s = String::with_capacity(v.len() * 2);
    for b in v {
        s.push(digits[(b >> 4) as usize] as char);
        s.push(digits[(b & 15) as usize] as char);
    }
    s
}

/// RFC 3986 percent-encoding, unreserved characters kept. Uppercase hex, the
/// form RFC 3986 recommends and most encoders emit.
fn percent(v: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len());
    for &b in v {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b);
        } else {
            out.push(b'%');
            out.push(b"0123456789ABCDEF"[(b >> 4) as usize]);
            out.push(b"0123456789ABCDEF"[(b & 15) as usize]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(secrets: &[(&str, &[u8])]) -> Redactor {
        Redactor::from_plaintexts(
            secrets
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_vec()))
                .collect(),
        )
        .unwrap()
    }

    const S: &[u8] = b"s3cr3t-Pa$$w0rd!";

    fn assert_clean(red: &Redactor, text: &str) {
        let out = red.redact(text);
        assert!(out.contains("[REDACTED secret:pw]"), "not redacted: {out}");
    }

    #[test]
    fn raw_and_embedded() {
        let red = r(&[("pw", S)]);
        assert_eq!(red.redact("s3cr3t-Pa$$w0rd!"), "[REDACTED secret:pw]");
        assert_eq!(
            red.redact("a=s3cr3t-Pa$$w0rd! b"),
            "a=[REDACTED secret:pw] b"
        );
        assert_eq!(red.redact("nothing here"), "nothing here");
    }

    #[test]
    fn base64_any_alignment_and_context() {
        use base64::engine::general_purpose::{STANDARD, URL_SAFE};
        let red = r(&[("pw", S)]);
        for plen in 0..7 {
            for slen in 0..7 {
                let mut blob = vec![b'x'; plen];
                blob.extend_from_slice(S);
                blob.extend(std::iter::repeat_n(b'y', slen));
                for enc in [
                    STANDARD.encode(&blob),
                    STANDARD_NO_PAD.encode(&blob),
                    URL_SAFE.encode(&blob),
                    URL_SAFE_NO_PAD.encode(&blob),
                ] {
                    let out = red.redact(&format!("Authorization: Basic {enc} end"));
                    assert!(out.contains("[REDACTED"), "p={plen} s={slen}: {out}");
                    assert!(out.starts_with("Authorization: Basic ") && out.ends_with(" end"));
                }
            }
        }
    }

    #[test]
    fn basic_auth_header() {
        let red = r(&[("pw", S)]);
        let enc =
            base64::engine::general_purpose::STANDARD.encode([b"user:".as_slice(), S].concat());
        assert_clean(&red, &format!("Basic {enc}"));
    }

    #[test]
    fn hex_both_cases() {
        let red = r(&[("pw", S)]);
        assert_eq!(red.redact(&hex(S, false)), "[REDACTED secret:pw]");
        assert_eq!(red.redact(&hex(S, true)), "[REDACTED secret:pw]");
    }

    #[test]
    fn percent_encoded() {
        let red = r(&[("pw", S)]);
        let out = red.redact("https://u:s3cr3t-Pa%24%24w0rd%21@host/");
        assert_eq!(out, "https://u:[REDACTED secret:pw]@host/");
    }

    #[test]
    fn json_escaped() {
        let v = b"pa\"ss\\wo\nrd-x";
        let red = r(&[("pw", v)]);
        let json = serde_json::to_string(std::str::from_utf8(v).unwrap()).unwrap();
        let out = red.redact(&format!("{{\"password\":{json}}}"));
        assert_eq!(out, "{\"password\":\"[REDACTED secret:pw]\"}");
    }

    #[test]
    fn multiline_single_line_leak() {
        let v = b"-----BEGIN KEY-----\nAAAABBBBCCCCDDDD\n-----END KEY-----\n";
        let red = r(&[("key", v)]);
        let out = red.redact("log: AAAABBBBCCCCDDDD done");
        assert_eq!(out, "log: [REDACTED secret:key] done");
    }

    #[test]
    fn short_secrets_skipped_and_reported() {
        let red = r(&[("tiny", b"abc"), ("pw", S)]);
        assert_eq!(red.too_short, vec!["tiny".to_string()]);
        assert_eq!(red.redact("abc abc"), "abc abc");
    }

    #[test]
    fn overlapping_substring_secrets() {
        let red = r(&[("long", b"hunter2hunter2"), ("short", b"hunter2")]);
        assert_eq!(
            red.redact("x hunter2hunter2 y"),
            "x [REDACTED secret:long] y"
        );
        assert_eq!(red.redact("x hunter2 y"), "x [REDACTED secret:short] y");
    }

    #[test]
    fn non_utf8_bytes() {
        let v = [0xff, 0xfe, 0x00, 0x41, 0x42, 0x43, 0xfd];
        let red = r(&[("bin", &v)]);
        let mut data = b"head".to_vec();
        data.extend_from_slice(&v);
        data.extend_from_slice(b"tail");
        assert_eq!(red.redact_bytes(&data), b"head[REDACTED secret:bin]tail");
    }

    #[test]
    fn debug_hides_values() {
        let red = r(&[("pw", S)]);
        let dbg = format!("{red:?}");
        assert!(!dbg.contains("s3cr3t"));
        assert!(dbg.contains("needles"));
    }

    #[test]
    fn from_state_flags_wrong_key() {
        let key = ClusterKey::generate();
        let other = ClusterKey::generate();
        let mut state = ClusterState::default();
        let mk = |k: &ClusterKey, name: &str, v: &[u8]| cl_raft::SecretEntry {
            name: name.into(),
            sealed: Sealed::seal(k, name, v).unwrap().blob,
            revision: 1,
            key_fingerprint: k.fingerprint(),
            owner: None,
        };
        state.secrets.insert("good".into(), mk(&key, "good", S));
        state.secrets.insert("bad".into(), mk(&other, "bad", S));
        let red = Redactor::from_state(&state, &key).unwrap();
        assert_eq!(red.unredactable, vec!["bad".to_string()]);
        assert_eq!(red.redact("s3cr3t-Pa$$w0rd!"), "[REDACTED secret:good]");
    }
}
