//! Human-facing device identifiers.
//!
//! A device ID is a server-assigned 9-digit number drawn at random from a
//! sparse space; it is never sequential, so it cannot be guessed by counting.
//! For display and entry we append one **Damm check digit**, giving a 10-digit
//! code. The Damm algorithm catches every single-digit typo and every
//! transposition of two adjacent digits, so a mistyped ID is rejected locally
//! instead of probing a stranger's device. The check digit is presentation
//! only: the stored [`DeviceId`] stays the bare 9-digit number.
//!
//! Enumeration resistance comes from three layers together: the sparse random
//! draw here, server-side per-IP rate limiting on connect, and binding each ID
//! to the device's Ed25519 identity key (see [`crate::signal`]) so a guessed
//! ID still cannot be claimed or impersonated.

use crate::signal::DeviceId;

/// Smallest/largest 9-digit value, so every ID formats to exactly 9 digits.
pub const MIN_ID: DeviceId = 100_000_000;
pub const MAX_ID: DeviceId = 999_999_999;

/// Damm quasigroup operation table (a weakly totally anti-symmetric
/// quasigroup of order 10). `DAMM[row][col]`.
const DAMM: [[u8; 10]; 10] = [
    [0, 3, 1, 7, 5, 9, 8, 6, 4, 2],
    [7, 0, 9, 2, 1, 5, 4, 8, 6, 3],
    [4, 2, 0, 6, 8, 7, 1, 3, 5, 9],
    [1, 7, 5, 0, 9, 8, 3, 4, 2, 6],
    [6, 1, 2, 3, 0, 4, 5, 9, 7, 8],
    [3, 6, 7, 4, 2, 0, 9, 5, 8, 1],
    [5, 8, 6, 9, 7, 2, 0, 1, 3, 4],
    [8, 9, 4, 5, 3, 6, 2, 0, 1, 7],
    [9, 4, 3, 8, 6, 1, 7, 2, 0, 5],
    [2, 5, 8, 1, 4, 3, 6, 7, 9, 0],
];

/// Running Damm checksum over `digits`. Zero means the sequence (including its
/// trailing check digit) is valid.
fn damm(digits: impl Iterator<Item = u8>) -> u8 {
    let mut interim = 0u8;
    for d in digits {
        interim = DAMM[interim as usize][d as usize];
    }
    interim
}

/// The check digit that makes `id`'s 9 digits a valid Damm sequence.
fn check_digit(id: DeviceId) -> u8 {
    let s = format!("{id:09}");
    damm(s.bytes().map(|b| b - b'0'))
}

/// Format an ID for display: 10 digits (9 + check) grouped as `123 456 7890`.
pub fn format(id: DeviceId) -> String {
    let id = id.clamp(MIN_ID, MAX_ID);
    let c = check_digit(id);
    let s = format!("{id:09}{c}");
    format!("{} {} {}", &s[0..3], &s[3..6], &s[6..10])
}

/// Parse a user-entered ID. Accepts any spacing/dashes, requires exactly 10
/// digits, and rejects the input unless the Damm check digit is valid.
pub fn parse(input: &str) -> Option<DeviceId> {
    let digits: Vec<u8> = input
        .chars()
        .filter(|c| c.is_ascii_digit())
        .map(|c| c as u8 - b'0')
        .collect();
    if digits.len() != 10 {
        return None;
    }
    if damm(digits.iter().copied()) != 0 {
        return None;
    }
    let id: DeviceId = input
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(9)
        .collect::<String>()
        .parse()
        .ok()?;
    (MIN_ID..=MAX_ID).contains(&id).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_parse_round_trip() {
        for id in [MIN_ID, MAX_ID, 123_456_789, 500_000_000, 100_000_001] {
            let shown = format(id);
            assert_eq!(parse(&shown), Some(id), "round trip {id} via {shown:?}");
        }
    }

    #[test]
    fn spacing_is_ignored() {
        let shown = format(123_456_789);
        let digits: String = shown.chars().filter(|c| c.is_ascii_digit()).collect();
        assert_eq!(parse(&digits), Some(123_456_789));
        assert_eq!(parse(&format!("  {shown}  ")), Some(123_456_789));
        let dashed = digits
            .chars()
            .enumerate()
            .flat_map(|(i, c)| if i == 3 || i == 6 { vec!['-', c] } else { vec![c] })
            .collect::<String>();
        assert_eq!(parse(&dashed), Some(123_456_789));
    }

    #[test]
    fn single_digit_typo_rejected() {
        let shown: String = format(123_456_789).chars().filter(|c| c.is_ascii_digit()).collect();
        // Flip each digit to every other value; a valid Damm sequence must
        // never result from a single-digit change.
        for i in 0..shown.len() {
            let orig = shown.as_bytes()[i];
            for d in b'0'..=b'9' {
                if d == orig {
                    continue;
                }
                let mut bad = shown.clone().into_bytes();
                bad[i] = d;
                let bad = String::from_utf8(bad).unwrap();
                assert!(parse(&bad).is_none(), "typo at {i} -> {bad} wrongly accepted");
            }
        }
    }

    #[test]
    fn adjacent_transposition_rejected() {
        let shown: String = format(123_456_789).chars().filter(|c| c.is_ascii_digit()).collect();
        let b = shown.as_bytes();
        for i in 0..b.len() - 1 {
            if b[i] == b[i + 1] {
                continue;
            }
            let mut sw = b.to_vec();
            sw.swap(i, i + 1);
            let sw = String::from_utf8(sw).unwrap();
            assert!(parse(&sw).is_none(), "transposition at {i} -> {sw} wrongly accepted");
        }
    }

    #[test]
    fn wrong_length_rejected() {
        assert_eq!(parse("123"), None);
        assert_eq!(parse("12345678901"), None);
        assert_eq!(parse(""), None);
    }
}
