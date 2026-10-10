//! The three ways to produce a secret's plaintext, matching the shapes
//! `PUT /api/secrets/:name` accepts. Sealing happens one layer up, in
//! `routes::objects`, so this module never touches the cluster key.

use serde::{Deserialize, Deserializer, Serialize};

/// Longest value the generator will produce. A generated secret is meant to
/// be a password or token, not a blob; anything bigger should come in as a
/// file upload instead.
pub const MAX_GENERATED_LEN: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Charset {
    Alphanumeric,
    Hex,
    Base64,
    Printable,
}

impl Charset {
    fn alphabet(self) -> &'static [u8] {
        match self {
            Charset::Alphanumeric => {
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
            }
            Charset::Hex => b"0123456789abcdef",
            // Standard base64 alphabet. The result is not padded and is not
            // meant to be decoded back to bytes, just used as a dense charset.
            Charset::Base64 => {
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
            }
            // Printable ASCII minus space and backslash/quote characters that
            // tend to cause trouble in shells and env files.
            Charset::Printable => {
                br##"!#$%&()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[]^_abcdefghijklmnopqrstuvwxyz{|}~"##
            }
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GenerateError {
    #[error("length must be at least 1")]
    TooShort,
    #[error("length must be at most {MAX_GENERATED_LEN}")]
    TooLong,
    #[error("no character class selected, so the alphabet is empty")]
    EmptyAlphabet,
    #[error("length {length} is shorter than the {classes} character classes required")]
    TooShortForClasses { length: usize, classes: usize },
    #[error("symbols must be printable non-whitespace ASCII")]
    BadSymbols,
    #[error("could not generate a value containing every required class")]
    ClassesNotSatisfied,
}

/// Generate a random secret value from a cryptographically secure RNG.
pub fn generate(length: usize, charset: Charset) -> Result<Vec<u8>, GenerateError> {
    if length == 0 {
        return Err(GenerateError::TooShort);
    }
    if length > MAX_GENERATED_LEN {
        return Err(GenerateError::TooLong);
    }

    Ok(sample(charset.alphabet(), length))
}

/// Draw `length` characters uniformly from `alphabet`.
fn sample(alphabet: &[u8], length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(length);
    // Reject-sampling one byte at a time keeps every character in the
    // alphabet equally likely, unlike `byte % len` which biases toward the
    // low end of the alphabet whenever len does not divide 256.
    let bound = (256 / alphabet.len()) * alphabet.len();
    while out.len() < length {
        let mut buf = [0u8; 32];
        rand::fill(&mut buf);
        for byte in buf {
            if out.len() == length {
                break;
            }
            if (byte as usize) < bound {
                out.push(alphabet[byte as usize % alphabet.len()]);
            }
        }
    }
    out
}

/// Which symbols a generated value may contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Symbols {
    None,
    /// The same safe set as `Charset::Printable`.
    Default,
    /// Exactly these characters.
    Custom(String),
}

impl<'de> Deserialize<'de> for Symbols {
    /// `false` -> none, `true` -> default set, a string -> that set. One field
    /// that reads naturally in a tool call, without a separate enum tag.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Text(String),
        }
        Ok(match Raw::deserialize(d)? {
            Raw::Bool(false) => Symbols::None,
            Raw::Bool(true) => Symbols::Default,
            Raw::Text(s) => Symbols::Custom(s),
        })
    }
}

impl Serialize for Symbols {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Symbols::None => s.serialize_bool(false),
            Symbols::Default => s.serialize_bool(true),
            Symbols::Custom(c) => s.serialize_str(c),
        }
    }
}

/// Fine-grained generator options, for callers (the MCP server) that need more
/// control than a single `Charset`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GenerateOptions {
    pub length: usize,
    pub lowercase: bool,
    pub uppercase: bool,
    pub digits: bool,
    pub symbols: Symbols,
    /// Drop `0O1lI|`, which are easy to misread when copied by hand.
    pub exclude_ambiguous: bool,
    /// Retry until every selected class appears.
    pub require_each_class: bool,
}

const AMBIGUOUS: &[u8] = b"0O1lI|";
const DEFAULT_SYMBOLS: &[u8] = br##"!#$%&()*+,-./:;<=>?@[]^_{|}~"##;
const MAX_CLASS_ATTEMPTS: usize = 1000;

/// Generate a secret from per-class options.
pub fn generate_with(opts: &GenerateOptions) -> Result<Vec<u8>, GenerateError> {
    if opts.length == 0 {
        return Err(GenerateError::TooShort);
    }
    if opts.length > MAX_GENERATED_LEN {
        return Err(GenerateError::TooLong);
    }

    let symbols: Vec<u8> = match &opts.symbols {
        Symbols::None => Vec::new(),
        Symbols::Default => DEFAULT_SYMBOLS.to_vec(),
        Symbols::Custom(c) => {
            if !c.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(GenerateError::BadSymbols);
            }
            let mut v = Vec::new();
            for b in c.bytes() {
                if !v.contains(&b) {
                    v.push(b);
                }
            }
            v
        }
    };
    let classes: [(bool, &[u8]); 4] = [
        (opts.lowercase, b"abcdefghijklmnopqrstuvwxyz"),
        (opts.uppercase, b"ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
        (opts.digits, b"0123456789"),
        (!symbols.is_empty(), &symbols),
    ];
    // Each class filtered on its own so `require_each_class` cannot demand a
    // class whose every member was excluded as ambiguous.
    let selected: Vec<Vec<u8>> = classes
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, set)| {
            set.iter()
                .copied()
                .filter(|b| !(opts.exclude_ambiguous && AMBIGUOUS.contains(b)))
                .collect::<Vec<u8>>()
        })
        .filter(|set| !set.is_empty())
        .collect();
    if selected.is_empty() {
        return Err(GenerateError::EmptyAlphabet);
    }
    if opts.require_each_class && opts.length < selected.len() {
        return Err(GenerateError::TooShortForClasses {
            length: opts.length,
            classes: selected.len(),
        });
    }
    let mut alphabet: Vec<u8> = selected.concat();
    alphabet.sort_unstable();
    alphabet.dedup();

    if !opts.require_each_class {
        return Ok(sample(&alphabet, opts.length));
    }
    // Regenerate whole values rather than forcing characters into positions,
    // which would skew the distribution and make the layout predictable.
    for _ in 0..MAX_CLASS_ATTEMPTS {
        let value = sample(&alphabet, opts.length);
        if selected
            .iter()
            .all(|set| value.iter().any(|b| set.contains(b)))
        {
            return Ok(value);
        }
    }
    Err(GenerateError::ClassesNotSatisfied)
}

/// Non-secret properties of a stored value, so an AI can diagnose "wrong
/// password" style problems (stray newline, trailing space) without ever
/// seeing the value. Deliberately no hash, prefix or suffix: those would leak
/// information about the value.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SecretFacts {
    pub byte_len: usize,
    /// `None` when the value is not UTF-8.
    pub char_len: Option<usize>,
    pub utf8: bool,
    pub leading_whitespace: bool,
    pub trailing_whitespace: bool,
    pub trailing_newline: bool,
    pub contains_newline: bool,
    pub contains_internal_whitespace: bool,
    pub has_lowercase: bool,
    pub has_uppercase: bool,
    pub has_digit: bool,
    pub has_symbol: bool,
    pub has_non_ascii: bool,
    pub has_control_chars: bool,
}

pub fn facts(plaintext: &[u8]) -> SecretFacts {
    let text = std::str::from_utf8(plaintext).ok();
    let ws = |b: &u8| b.is_ascii_whitespace();
    let trimmed = match plaintext.iter().position(|b| !ws(b)) {
        None => &plaintext[0..0],
        Some(s) => {
            let e = plaintext.iter().rposition(|b| !ws(b)).unwrap();
            &plaintext[s..=e]
        }
    };
    SecretFacts {
        byte_len: plaintext.len(),
        char_len: text.map(|t| t.chars().count()),
        utf8: text.is_some(),
        leading_whitespace: plaintext.first().is_some_and(ws),
        trailing_whitespace: plaintext.last().is_some_and(ws),
        trailing_newline: plaintext.last() == Some(&b'\n'),
        contains_newline: plaintext.contains(&b'\n'),
        contains_internal_whitespace: trimmed.iter().any(ws),
        has_lowercase: plaintext.iter().any(u8::is_ascii_lowercase),
        has_uppercase: plaintext.iter().any(u8::is_ascii_uppercase),
        has_digit: plaintext.iter().any(u8::is_ascii_digit),
        has_symbol: plaintext.iter().any(u8::is_ascii_punctuation),
        has_non_ascii: plaintext.iter().any(|b| !b.is_ascii()),
        has_control_chars: plaintext.iter().any(u8::is_ascii_control),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_the_requested_length() {
        for len in [1, 16, 32, 4096] {
            let value = generate(len, Charset::Alphanumeric).unwrap();
            assert_eq!(value.len(), len);
        }
    }

    #[test]
    fn zero_length_is_refused() {
        assert_eq!(generate(0, Charset::Hex), Err(GenerateError::TooShort));
    }

    #[test]
    fn over_the_cap_is_refused() {
        assert_eq!(
            generate(MAX_GENERATED_LEN + 1, Charset::Hex),
            Err(GenerateError::TooLong)
        );
        assert!(generate(MAX_GENERATED_LEN, Charset::Hex).is_ok());
    }

    #[test]
    fn every_byte_is_in_the_charset() {
        for charset in [
            Charset::Alphanumeric,
            Charset::Hex,
            Charset::Base64,
            Charset::Printable,
        ] {
            let value = generate(256, charset).unwrap();
            let alphabet = charset.alphabet();
            for byte in &value {
                assert!(
                    alphabet.contains(byte),
                    "{byte} not in {charset:?} alphabet"
                );
            }
        }
    }

    #[test]
    fn two_calls_differ() {
        let a = generate(32, Charset::Alphanumeric).unwrap();
        let b = generate(32, Charset::Alphanumeric).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn hex_output_is_lowercase_hex() {
        let value = generate(64, Charset::Hex).unwrap();
        let text = String::from_utf8(value).unwrap();
        assert!(text
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    fn opts() -> GenerateOptions {
        GenerateOptions {
            length: 64,
            lowercase: true,
            uppercase: false,
            digits: false,
            symbols: Symbols::None,
            exclude_ambiguous: false,
            require_each_class: false,
        }
    }

    #[test]
    fn symbols_deserialize_from_bool_or_string() {
        assert_eq!(
            serde_json::from_str::<Symbols>("false").unwrap(),
            Symbols::None
        );
        assert_eq!(
            serde_json::from_str::<Symbols>("true").unwrap(),
            Symbols::Default
        );
        assert_eq!(
            serde_json::from_str::<Symbols>("\"!@\"").unwrap(),
            Symbols::Custom("!@".into())
        );
    }

    #[test]
    fn generate_with_respects_classes() {
        let v = generate_with(&opts()).unwrap();
        assert_eq!(v.len(), 64);
        assert!(v.iter().all(u8::is_ascii_lowercase));

        let v = generate_with(&GenerateOptions {
            lowercase: false,
            digits: true,
            ..opts()
        })
        .unwrap();
        assert!(v.iter().all(u8::is_ascii_digit));

        let v = generate_with(&GenerateOptions {
            lowercase: false,
            symbols: Symbols::Default,
            ..opts()
        })
        .unwrap();
        assert!(v.iter().all(|b| DEFAULT_SYMBOLS.contains(b)));
        assert!(!v.contains(&b'\\') && !v.contains(&b'"') && !v.contains(&b' '));
    }

    #[test]
    fn custom_symbols_are_used_and_deduplicated() {
        let v = generate_with(&GenerateOptions {
            lowercase: false,
            symbols: Symbols::Custom("ab\\ab".into()),
            ..opts()
        })
        .unwrap();
        assert!(v.iter().all(|b| b"ab\\".contains(b)));
        assert_eq!(
            generate_with(&GenerateOptions {
                symbols: Symbols::Custom("a b".into()),
                ..opts()
            }),
            Err(GenerateError::BadSymbols)
        );
    }

    #[test]
    fn exclude_ambiguous_drops_lookalikes() {
        let v = generate_with(&GenerateOptions {
            length: 2000,
            uppercase: true,
            digits: true,
            symbols: Symbols::Default,
            exclude_ambiguous: true,
            ..opts()
        })
        .unwrap();
        assert!(!v.iter().any(|b| AMBIGUOUS.contains(b)));
    }

    #[test]
    fn every_char_of_a_non_power_of_two_alphabet_appears() {
        // 26 chars does not divide 256; with reject sampling all still show up.
        let v = generate_with(&GenerateOptions {
            length: 4096,
            ..opts()
        })
        .unwrap();
        for c in b'a'..=b'z' {
            assert!(v.contains(&c));
        }
    }

    #[test]
    fn require_each_class_holds() {
        for _ in 0..50 {
            let v = generate_with(&GenerateOptions {
                length: 4,
                uppercase: true,
                digits: true,
                symbols: Symbols::Default,
                require_each_class: true,
                ..opts()
            })
            .unwrap();
            assert!(v.iter().any(u8::is_ascii_lowercase));
            assert!(v.iter().any(u8::is_ascii_uppercase));
            assert!(v.iter().any(u8::is_ascii_digit));
            assert!(v.iter().any(|b| DEFAULT_SYMBOLS.contains(b)));
        }
    }

    #[test]
    fn generate_with_errors() {
        let none = GenerateOptions {
            lowercase: false,
            ..opts()
        };
        assert_eq!(generate_with(&none), Err(GenerateError::EmptyAlphabet));
        assert_eq!(
            generate_with(&GenerateOptions {
                length: 0,
                ..opts()
            }),
            Err(GenerateError::TooShort)
        );
        assert_eq!(
            generate_with(&GenerateOptions {
                length: MAX_GENERATED_LEN + 1,
                ..opts()
            }),
            Err(GenerateError::TooLong)
        );
        assert_eq!(
            generate_with(&GenerateOptions {
                length: 1,
                uppercase: true,
                require_each_class: true,
                ..opts()
            }),
            Err(GenerateError::TooShortForClasses {
                length: 1,
                classes: 2
            })
        );
    }

    #[test]
    fn facts_describe_without_revealing() {
        let f = facts(b" Ab1!x\ny\n");
        assert_eq!(f.byte_len, 9);
        assert_eq!(f.char_len, Some(9));
        assert!(f.utf8 && f.leading_whitespace && f.trailing_whitespace);
        assert!(f.trailing_newline && f.contains_newline);
        assert!(f.has_lowercase && f.has_uppercase && f.has_digit && f.has_symbol);
        assert!(f.has_control_chars && !f.has_non_ascii);
        assert!(f.contains_internal_whitespace);
    }

    #[test]
    fn facts_plain_and_non_utf8() {
        let f = facts(b"abc");
        assert!(!f.leading_whitespace && !f.trailing_whitespace && !f.contains_internal_whitespace);
        assert!(!f.has_control_chars && !f.trailing_newline);
        let f = facts(&[0xff, 0x61]);
        assert!(!f.utf8 && f.char_len.is_none() && f.has_non_ascii);
        let f = facts("héllo".as_bytes());
        assert_eq!((f.byte_len, f.char_len), (6, Some(5)));
        assert!(f.has_non_ascii);
        let f = facts(b"");
        assert_eq!(f.byte_len, 0);
    }
}
