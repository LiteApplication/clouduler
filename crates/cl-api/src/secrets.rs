//! The three ways to produce a secret's plaintext, matching the shapes
//! `PUT /api/secrets/:name` accepts. Sealing happens one layer up, in
//! `routes::objects`, so this module never touches the cluster key.

use serde::Deserialize;

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
}

/// Generate a random secret value from a cryptographically secure RNG.
pub fn generate(length: usize, charset: Charset) -> Result<Vec<u8>, GenerateError> {
    if length == 0 {
        return Err(GenerateError::TooShort);
    }
    if length > MAX_GENERATED_LEN {
        return Err(GenerateError::TooLong);
    }

    let alphabet = charset.alphabet();
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
    Ok(out)
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
}
