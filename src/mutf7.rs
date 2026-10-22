//! IMAP's modified UTF-7 for mailbox names (RFC 3501 5.1.3): `Messages envoy&AOk-s`.

use base64::Engine;
use base64::alphabet::IMAP_MUTF7;
use base64::engine::general_purpose::{GeneralPurpose, NO_PAD};

const B64: GeneralPurpose = GeneralPurpose::new(&IMAP_MUTF7, NO_PAD);

pub fn decode(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(end) = after.find('-') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let chunk = &after[..end];
        if chunk.is_empty() {
            out.push('&');
        } else {
            match B64.decode(chunk) {
                Ok(bytes) => {
                    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                    out.push_str(&String::from_utf16_lossy(&units));
                }
                Err(_) => out.push_str(&rest[i..i + 2 + end]),
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

pub fn encode(s: &str) -> String {
    let mut out = String::new();
    let mut pending: Vec<u16> = Vec::new();
    let flush = |pending: &mut Vec<u16>, out: &mut String| {
        if !pending.is_empty() {
            let bytes: Vec<u8> = pending.iter().flat_map(|u| u.to_be_bytes()).collect();
            out.push('&');
            out.push_str(&B64.encode(bytes));
            out.push('-');
            pending.clear();
        }
    };
    for c in s.chars() {
        if (' '..='~').contains(&c) {
            flush(&mut pending, &mut out);
            if c == '&' {
                out.push_str("&-");
            } else {
                out.push(c);
            }
        } else {
            let mut buf = [0u16; 2];
            pending.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush(&mut pending, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        assert_eq!(decode("[Gmail]/Messages envoy&AOk-s"), "[Gmail]/Messages envoyés");
        assert_eq!(encode("[Gmail]/Messages envoyés"), "[Gmail]/Messages envoy&AOk-s");
        assert_eq!(decode("A&-B"), "A&B");
        assert_eq!(encode("A&B"), "A&-B");
        for s in ["Éléments envoyés", "日本語", "plain", "a😀b"] {
            assert_eq!(decode(&encode(s)), s);
        }
    }
}
