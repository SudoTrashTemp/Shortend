//! Checksums: the algorithm menu, digests, and a multi-algorithm single-pass hasher.

mod c4;

use std::fmt;
use std::str::FromStr;

use md5::Digest as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgorithm {
    Xxh64,
    Xxh3,
    Xxh128,
    Md5,
    Sha1,
    Sha256,
    C4,
}

impl HashAlgorithm {
    pub const ALL: [HashAlgorithm; 7] = [
        Self::Xxh64,
        Self::Xxh3,
        Self::Xxh128,
        Self::Md5,
        Self::Sha1,
        Self::Sha256,
        Self::C4,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Xxh64 => "xxh64",
            Self::Xxh3 => "xxh3",
            Self::Xxh128 => "xxh128",
            Self::Md5 => "md5",
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::C4 => "c4",
        }
    }

    /// Raw digest length in bytes.
    pub fn digest_len(self) -> usize {
        match self {
            Self::Xxh64 | Self::Xxh3 => 8,
            Self::Xxh128 | Self::Md5 => 16,
            Self::Sha1 => 20,
            Self::Sha256 => 32,
            Self::C4 => 64,
        }
    }
}

impl fmt::Display for HashAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for HashAlgorithm {
    type Err = HashParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.to_ascii_lowercase();
        let alias = match lower.as_str() {
            "xxhash64" | "xxh64" => "xxh64",
            "xxh3_64" | "xxh3" => "xxh3",
            "xxh3_128" | "xxh128" => "xxh128",
            "sha-1" => "sha1",
            "sha-256" => "sha256",
            other => other,
        };
        Self::ALL
            .into_iter()
            .find(|a| a.name() == alias)
            .ok_or_else(|| HashParseError::UnknownAlgorithm(s.to_owned()))
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HashParseError {
    #[error(
        "unknown checksum algorithm '{0}' (expected one of xxh64, xxh3, xxh128, md5, sha1, sha256, c4)"
    )]
    UnknownAlgorithm(String),
    #[error("malformed {0} digest '{1}'")]
    Malformed(HashAlgorithm, String),
    #[error("a job needs at least one checksum algorithm")]
    Empty,
    #[error("checksum algorithm {0} listed twice")]
    Duplicate(HashAlgorithm),
}

/// A finished checksum. Text form is lowercase hex, except C4 which uses its own base58 ID.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Digest {
    algo: HashAlgorithm,
    bytes: Box<[u8]>,
}

impl Digest {
    pub fn algo(&self) -> HashAlgorithm {
        self.algo
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The value as written in reports and manifests (hex, or a `c4…` ID).
    pub fn value(&self) -> String {
        match self.algo {
            HashAlgorithm::C4 => {
                let arr: &[u8; 64] = self.bytes.as_ref().try_into().expect("c4 is 64 bytes");
                c4::encode(arr)
            }
            _ => self.bytes.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }

    pub fn parse(algo: HashAlgorithm, s: &str) -> Result<Self, HashParseError> {
        let bad = || HashParseError::Malformed(algo, s.to_owned());
        let bytes: Box<[u8]> = match algo {
            HashAlgorithm::C4 => Box::new(c4::decode(s).ok_or_else(bad)?),
            _ => {
                if s.len() != algo.digest_len() * 2 || !s.is_ascii() {
                    return Err(bad());
                }
                (0..s.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| bad()))
                    .collect::<Result<_, _>>()?
            }
        };
        Ok(Self { algo, bytes })
    }
}

impl fmt::Display for Digest {
    /// `algo:value`, e.g. `xxh64:ef46db3751d8e999`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algo, self.value())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for Digest {
    type Err = HashParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (algo, value) = s
            .split_once(':')
            .ok_or_else(|| HashParseError::UnknownAlgorithm(s.to_owned()))?;
        Digest::parse(algo.parse()?, value)
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Non-empty, duplicate-free, ordered list of algorithms. The first entry is the primary.
/// (Named `HashList` rather than `HashSet` to avoid clashing with `std`.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct HashList(Vec<HashAlgorithm>);

impl HashList {
    pub fn new(algos: Vec<HashAlgorithm>) -> Result<Self, HashParseError> {
        if algos.is_empty() {
            return Err(HashParseError::Empty);
        }
        for (i, a) in algos.iter().enumerate() {
            if algos[..i].contains(a) {
                return Err(HashParseError::Duplicate(*a));
            }
        }
        Ok(Self(algos))
    }

    pub fn single(algo: HashAlgorithm) -> Self {
        Self(vec![algo])
    }

    pub fn primary(&self) -> HashAlgorithm {
        self.0[0]
    }

    pub fn as_slice(&self) -> &[HashAlgorithm] {
        &self.0
    }
}

impl Default for HashList {
    fn default() -> Self {
        Self::single(HashAlgorithm::Xxh64)
    }
}

impl<'de> Deserialize<'de> for HashList {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        HashList::new(Vec::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for HashList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, a) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("+")?;
            }
            write!(f, "{a}")?;
        }
        Ok(())
    }
}

/// One digest per algorithm of a [`HashList`], in the same order.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digests(Vec<Digest>);

impl Digests {
    pub fn primary(&self) -> &Digest {
        &self.0[0]
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Digest> {
        self.0.iter()
    }

    /// True when both carry the same algorithms in the same order.
    pub fn same_algorithms(&self, list: &HashList) -> bool {
        self.0.len() == list.0.len() && self.0.iter().zip(&list.0).all(|(d, a)| d.algo == *a)
    }
}

impl fmt::Display for Digests {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, d) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{d}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Digests {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A single in-progress checksum. An enum rather than a trait object: the set is closed and
/// the per-chunk `update` stays a direct call.
pub enum Hasher {
    Xxh64(xxhash_rust::xxh64::Xxh64),
    Xxh3(Box<xxhash_rust::xxh3::Xxh3>),
    Xxh128(Box<xxhash_rust::xxh3::Xxh3>),
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    C4(sha2::Sha512),
}

impl Hasher {
    pub fn new(algo: HashAlgorithm) -> Self {
        match algo {
            HashAlgorithm::Xxh64 => Self::Xxh64(xxhash_rust::xxh64::Xxh64::new(0)),
            HashAlgorithm::Xxh3 => Self::Xxh3(Box::default()),
            HashAlgorithm::Xxh128 => Self::Xxh128(Box::default()),
            HashAlgorithm::Md5 => Self::Md5(md5::Md5::new()),
            HashAlgorithm::Sha1 => Self::Sha1(sha1::Sha1::new()),
            HashAlgorithm::Sha256 => Self::Sha256(sha2::Sha256::new()),
            HashAlgorithm::C4 => Self::C4(sha2::Sha512::new()),
        }
    }

    pub fn update(&mut self, buf: &[u8]) {
        match self {
            Self::Xxh64(h) => h.update(buf),
            Self::Xxh3(h) | Self::Xxh128(h) => h.update(buf),
            Self::Md5(h) => h.update(buf),
            Self::Sha1(h) => h.update(buf),
            Self::Sha256(h) => h.update(buf),
            Self::C4(h) => h.update(buf),
        }
    }

    pub fn finish(self) -> Digest {
        let (algo, bytes): (HashAlgorithm, Box<[u8]>) = match self {
            Self::Xxh64(h) => (HashAlgorithm::Xxh64, Box::new(h.digest().to_be_bytes())),
            Self::Xxh3(h) => (HashAlgorithm::Xxh3, Box::new(h.digest().to_be_bytes())),
            Self::Xxh128(h) => (HashAlgorithm::Xxh128, Box::new(h.digest128().to_be_bytes())),
            Self::Md5(h) => (HashAlgorithm::Md5, h.finalize().to_vec().into()),
            Self::Sha1(h) => (HashAlgorithm::Sha1, h.finalize().to_vec().into()),
            Self::Sha256(h) => (HashAlgorithm::Sha256, h.finalize().to_vec().into()),
            Self::C4(h) => (HashAlgorithm::C4, h.finalize().to_vec().into()),
        };
        Digest { algo, bytes }
    }
}

/// Computes every algorithm of a [`HashList`] over the same bytes in one read pass.
pub struct MultiHasher(Vec<Hasher>);

impl MultiHasher {
    pub fn new(list: &HashList) -> Self {
        Self(list.0.iter().map(|&a| Hasher::new(a)).collect())
    }

    pub fn update(&mut self, buf: &[u8]) {
        for h in &mut self.0 {
            h.update(buf);
        }
    }

    pub fn finish(self) -> Digests {
        Digests(self.0.into_iter().map(Hasher::finish).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(algo: HashAlgorithm, data: &[u8]) -> String {
        let mut h = Hasher::new(algo);
        h.update(data);
        h.finish().value()
    }

    /// Deterministic test pattern; the Python generator used for the vectors is
    /// `bytes(((i*2654435761)>>13)&0xff for i in range(n))`.
    fn pattern(n: usize) -> Vec<u8> {
        (0..n as u64)
            .map(|i| ((i * 2_654_435_761) >> 13) as u8)
            .collect()
    }

    // Reference values produced with Python `hashlib`, the `xxhash` package and the ASC MHL
    // reference library's C4 implementation (`ascmhl.hasher.C4`).
    const VECTORS: &[(&str, [&str; 7])] = &[
        (
            "",
            [
                "ef46db3751d8e999",
                "2d06800538d394c2",
                "99aa06d3014798d86001c324468d497f",
                "d41d8cd98f00b204e9800998ecf8427e",
                "da39a3ee5e6b4b0d3255bfef95601890afd80709",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                "c459dsjfscH38cYeXXYogktxf4Cd9ibshE3BHUo6a58hBXmRQdZrAkZzsWcbWtDg5oQstpDuni4Hirj75GEmTc1sFT",
            ],
        ),
        (
            "abc",
            [
                "44bc2cf5ad770999",
                "78af5f94892f3950",
                "06b05ab6733a618578af5f94892f3950",
                "900150983cd24fb0d6963f7d28e17f72",
                "a9993e364706816aba3e25717850c26c9cd0d89d",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                "c45S4rnaTNWonxss1u8LzsaJdEph1AJhWUF4sh2waXKMsutyfAxg4ybUeuXVWS9HdNcEypmeXn8FZGonD4w1rj9DZp",
            ],
        ),
    ];

    const PATTERN_VECTORS: [&str; 7] = [
        "09eb56443d16247d",
        "a80bea05304486e6",
        "8119ab399ae5fb2ca80bea05304486e6",
        "6afe401dad84773b8e640ec0d322f1f2",
        "f3d9965388648575d63e719d65b11d7104f844de",
        "db2f6ec1b4ef97a0d56297478b3adc5fe7d11a2a648d181c8d331ac4d8ab09dc",
        "c43LwJjEJfRBUpm3BwZJQaKjBsb8mAxFXDpiZUkuMQH4ffD1HZMnvEAwBmgNmYmGi8rgtUY87k7PYjg5VyWvz4YJ9R",
    ];

    #[test]
    fn known_vectors() {
        for (input, expected) in VECTORS {
            for (algo, want) in HashAlgorithm::ALL.into_iter().zip(expected) {
                assert_eq!(hash(algo, input.as_bytes()), *want, "{algo} of {input:?}");
            }
        }
    }

    #[test]
    fn streaming_matches_one_shot_at_odd_boundaries() {
        let data = pattern(1_048_576 + 7);
        for (algo, want) in HashAlgorithm::ALL.into_iter().zip(PATTERN_VECTORS) {
            assert_eq!(hash(algo, &data), want, "{algo} one-shot");
            for chunk in [1usize, 7, 4096, 65_537, 1_000_003] {
                let mut h = Hasher::new(algo);
                for part in data.chunks(chunk) {
                    h.update(part);
                }
                assert_eq!(h.finish().value(), want, "{algo} in {chunk}-byte chunks");
            }
        }
    }

    #[test]
    fn multi_hasher_computes_all_in_one_pass() {
        let list = HashList::new(HashAlgorithm::ALL.to_vec()).unwrap();
        let mut m = MultiHasher::new(&list);
        for part in pattern(1_048_576 + 7).chunks(8192) {
            m.update(part);
        }
        let digests = m.finish();
        assert!(digests.same_algorithms(&list));
        let got: Vec<String> = digests.iter().map(Digest::value).collect();
        assert_eq!(got, PATTERN_VECTORS);
    }

    #[test]
    fn digest_text_round_trip() {
        for algo in HashAlgorithm::ALL {
            let mut h = Hasher::new(algo);
            h.update(b"round trip");
            let d = h.finish();
            let text = d.to_string();
            assert_eq!(text.parse::<Digest>().unwrap(), d, "{text}");
            let json = serde_json::to_string(&d).unwrap();
            assert_eq!(serde_json::from_str::<Digest>(&json).unwrap(), d);
        }
        assert!(Digest::parse(HashAlgorithm::Xxh64, "zz46db3751d8e999").is_err());
        assert!(Digest::parse(HashAlgorithm::Xxh64, "ef46").is_err());
        assert!(Digest::parse(HashAlgorithm::C4, "c4short").is_err());
    }

    #[test]
    fn hash_list_rules() {
        assert_eq!(HashList::new(vec![]), Err(HashParseError::Empty));
        assert_eq!(
            HashList::new(vec![HashAlgorithm::Md5, HashAlgorithm::Md5]),
            Err(HashParseError::Duplicate(HashAlgorithm::Md5))
        );
        assert_eq!(HashList::default().primary(), HashAlgorithm::Xxh64);
        assert_eq!("XXHASH64".parse(), Ok(HashAlgorithm::Xxh64));
        assert!("crc32".parse::<HashAlgorithm>().is_err());
    }
}
