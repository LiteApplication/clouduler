//! A byte count that can be written as a number or as a human size.
//!
//! `max_body_size: 10MB` in a stack file is the point of this: an operator
//! reasons in megabytes and should not have to count zeroes, but the proxy
//! needs an exact `u64` and a round trip through yaml must not change it.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A size in bytes. Accepts `1048576`, `"1MiB"`, `"1MB"`, `"10 kb"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSize(pub u64);

impl ByteSize {
    pub fn bytes(self) -> u64 {
        self.0
    }
}

/// Decimal and binary suffixes both accepted, longest first so `mib` is not
/// matched as `m`. Ordering matters: `b` has to come last or it swallows the
/// `b` in `kb`.
const UNITS: &[(&str, u64)] = &[
    ("kib", 1024),
    ("mib", 1024 * 1024),
    ("gib", 1024 * 1024 * 1024),
    ("tib", 1024u64 * 1024 * 1024 * 1024),
    ("kb", 1_000),
    ("mb", 1_000_000),
    ("gb", 1_000_000_000),
    ("tb", 1_000_000_000_000),
    ("k", 1024),
    ("m", 1024 * 1024),
    ("g", 1024 * 1024 * 1024),
    ("t", 1024u64 * 1024 * 1024 * 1024),
    ("b", 1),
];

impl std::str::FromStr for ByteSize {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let text = raw.trim().to_ascii_lowercase();
        if text.is_empty() {
            return Err("empty size".to_string());
        }
        // A bare number is bytes.
        if let Ok(n) = text.parse::<u64>() {
            return Ok(ByteSize(n));
        }

        for (suffix, factor) in UNITS {
            let Some(number) = text.strip_suffix(suffix) else {
                continue;
            };
            let number = number.trim();
            let value: f64 = number
                .parse()
                .map_err(|_| format!("{raw}: {number} is not a number"))?;
            if value < 0.0 {
                return Err(format!("{raw}: a size cannot be negative"));
            }
            let bytes = value * (*factor as f64);
            if bytes > u64::MAX as f64 {
                return Err(format!("{raw}: too large"));
            }
            return Ok(ByteSize(bytes as u64));
        }
        Err(format!("{raw}: not a size, try 10MB or 1048576"))
    }
}

impl fmt::Display for ByteSize {
    /// Renders back to the largest binary unit that divides exactly, so a
    /// config that was written as `10MiB` comes back out that way rather than
    /// as a wall of digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (suffix, factor) in [("GiB", 1024u64 * 1024 * 1024), ("MiB", 1024 * 1024), ("KiB", 1024)] {
            if self.0 >= factor && self.0.is_multiple_of(factor) {
                return write!(f, "{}{suffix}", self.0 / factor);
            }
        }
        write!(f, "{}", self.0)
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Untagged so both `10MB` and a bare `1048576` parse.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Int(n) => Ok(ByteSize(n)),
            Raw::Text(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_numbers_as_bytes() {
        assert_eq!("1048576".parse::<ByteSize>().unwrap(), ByteSize(1048576));
    }

    #[test]
    fn parses_decimal_and_binary_suffixes() {
        assert_eq!("1MB".parse::<ByteSize>().unwrap(), ByteSize(1_000_000));
        assert_eq!("1MiB".parse::<ByteSize>().unwrap(), ByteSize(1024 * 1024));
        assert_eq!("1M".parse::<ByteSize>().unwrap(), ByteSize(1024 * 1024));
        assert_eq!("10 kb".parse::<ByteSize>().unwrap(), ByteSize(10_000));
    }

    /// `kb` must not be read as `k` followed by junk, nor `b` alone.
    #[test]
    fn longer_suffixes_win_over_their_prefixes() {
        assert_eq!("2kb".parse::<ByteSize>().unwrap(), ByteSize(2_000));
        assert_eq!("2kib".parse::<ByteSize>().unwrap(), ByteSize(2048));
        assert_eq!("2b".parse::<ByteSize>().unwrap(), ByteSize(2));
    }

    #[test]
    fn fractional_sizes_are_allowed() {
        assert_eq!("1.5MiB".parse::<ByteSize>().unwrap(), ByteSize(1_572_864));
    }

    #[test]
    fn rubbish_is_rejected() {
        assert!("".parse::<ByteSize>().is_err());
        assert!("big".parse::<ByteSize>().is_err());
        assert!("-1MB".parse::<ByteSize>().is_err());
    }

    #[test]
    fn round_trips_through_yaml() {
        let original = ByteSize(10 * 1024 * 1024);
        let encoded = serde_yaml::to_string(&original).unwrap();
        assert!(encoded.contains("10MiB"), "{encoded}");
        let decoded: ByteSize = serde_yaml::from_str(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn a_bare_integer_in_yaml_is_bytes() {
        let decoded: ByteSize = serde_yaml::from_str("4096").unwrap();
        assert_eq!(decoded, ByteSize(4096));
    }
}
