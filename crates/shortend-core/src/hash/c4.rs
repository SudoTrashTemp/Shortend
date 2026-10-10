//! C4 ID encoding (SMPTE ST 2114): a SHA-512 digest written as a fixed-width base58 string
//! prefixed with `c4`, 90 characters in total. Matches the ASC MHL reference implementation.

const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const ID_LEN: usize = 90;
const BODY_LEN: usize = ID_LEN - 2;

/// Encodes a 64-byte SHA-512 digest as a C4 ID.
pub fn encode(digest: &[u8; 64]) -> String {
    // Big-endian base-256 → base-58 by repeated division of the byte string.
    let mut num = *digest;
    let mut out = [b'1'; BODY_LEN];
    let mut pos = BODY_LEN;
    while num.iter().any(|&b| b != 0) {
        let mut rem: u32 = 0;
        for byte in &mut num {
            let acc = (rem << 8) | u32::from(*byte);
            *byte = (acc / 58) as u8;
            rem = acc % 58;
        }
        pos -= 1;
        out[pos] = ALPHABET[rem as usize];
    }
    let mut s = String::with_capacity(ID_LEN);
    s.push_str("c4");
    // ALPHABET is ASCII, so this is valid UTF-8.
    s.push_str(std::str::from_utf8(&out).expect("ascii"));
    s
}

/// Decodes a C4 ID back to the 64-byte SHA-512 digest.
pub fn decode(id: &str) -> Option<[u8; 64]> {
    let body = id.strip_prefix("c4")?;
    if body.len() != BODY_LEN {
        return None;
    }
    let mut num = [0u8; 64];
    for c in body.bytes() {
        let digit = ALPHABET.iter().position(|&a| a == c)? as u32;
        // num = num * 58 + digit, big-endian.
        let mut carry = digit;
        for byte in num.iter_mut().rev() {
            let acc = u32::from(*byte) * 58 + carry;
            *byte = (acc & 0xff) as u8;
            carry = acc >> 8;
        }
        if carry != 0 {
            return None; // overflow: not a valid 512-bit value
        }
    }
    Some(num)
}
