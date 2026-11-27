//! TOTP (RFC 6238), recovery codes and base32 helpers.

/// RFC 6238 TOTP value (HMAC-SHA1, 30 s step, 6 digits) at `unix_secs`.
pub fn totp_at(secret: &[u8], unix_secs: i64) -> u32 {
    let counter = (unix_secs / 30) as u64;
    let msg = counter.to_be_bytes();
    use hmac::Mac;
    let mut mac = <hmac::Hmac<sha1::Sha1> as hmac::Mac>::new_from_slice(secret)
        .expect("HMAC accepts any key length");
    mac.update(&msg);
    let out = mac.finalize().into_bytes();
    let offset = (out[19] & 0x0f) as usize;
    let binary = ((u32::from(out[offset]) & 0x7f) << 24)
        | ((u32::from(out[offset + 1])) << 16)
        | ((u32::from(out[offset + 2])) << 8)
        | u32::from(out[offset + 3]);
    binary % 1_000_000
}

/// The 30 s time-steps (ascending) among previous, current and next whose TOTP value is
/// `code`; empty for a wrong or non-numeric code. Login uses this with the per-user
/// last-accepted step to refuse replays (SEC-15).
pub fn totp_matching_steps(secret: &[u8], code: &str, unix_secs: i64) -> Vec<i64> {
    // Same acceptance as `totp_verify`.
    let Ok(code_num) = code.parse::<u32>() else {
        return Vec::new();
    };
    let base = unix_secs.div_euclid(30);
    [-1i64, 0, 1]
        .iter()
        .map(|d| base + d)
        .filter(|step| totp_at(secret, step * 30) == code_num)
        .collect()
}

/// Accept `code` if it matches the previous, current or next 30 s step (±1 step of clock
/// skew); non-numeric input is rejected. Stateless: login replay protection lives in
/// [`AuthService::verify_user_totp`] (SEC-15); enrollment confirmation uses this directly.
pub fn totp_verify(secret: &[u8], code: &str, unix_secs: i64) -> bool {
    let Ok(code_num) = code.parse::<u32>() else {
        return false;
    };
    [-30, 0, 30].iter().any(|step| {
        // Constant-shape comparison; the values are short-lived codes.
        totp_at(secret, unix_secs + step) == code_num
    })
}

pub(super) fn recovery_code() -> String {
    // XXXX-XXXX from a Crockford-style alphabet. Every character is
    // drawn from the OS CSPRNG (SEC-N2): the earlier UUID-v4 source
    // pinned character 7 to the version nibble (16 of 32 values).
    // 32 divides 256, so `% 32` is unbiased: 40 bits per code.
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("OS CSPRNG unavailable");
    let pick = |i: usize| ALPHABET[(bytes[i] % 32) as usize] as char;
    format!(
        "{}{}{}{}-{}{}{}{}",
        pick(0),
        pick(1),
        pick(2),
        pick(3),
        pick(4),
        pick(5),
        pick(6),
        pick(7)
    )
}

const B32: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32 without padding (the otpauth `secret` form).
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        buffer = (buffer << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Inverse of [`base32_encode`]. Uppercase alphabet only; `=` and whitespace are skipped
/// and any other character yields `None`.
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.chars() {
        if c == '=' || c.is_whitespace() {
            continue;
        }
        let val = B32.iter().position(|&x| x == c as u8)? as u32;
        buffer = (buffer << 5) | val;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totp_rfc_vectors() {
        // RFC 6238 test secret "12345678901234567890" (ASCII), SHA-1.
        let secret = b"12345678901234567890";
        assert_eq!(totp_at(secret, 59), 287082);
        assert_eq!(totp_at(secret, 1111111109), 81804);
        assert!(totp_verify(secret, "287082", 59));
        assert!(totp_verify(secret, "287082", 89)); // +1 step window
        assert!(!totp_verify(secret, "111111", 59));
        let b32 = base32_encode(secret);
        assert_eq!(base32_decode(&b32).unwrap(), secret.to_vec());
    }

    /// SEC-N2: recovery/enrollment codes keep the XXXX-XXXX format and
    /// every character position spans the full 32-symbol alphabet (the
    /// UUID-v4 source pinned position 7 to 16 values).
    #[test]
    fn recovery_code_uses_full_alphabet_in_every_position() {
        const ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut seen: Vec<std::collections::HashSet<char>> = vec![Default::default(); 9];
        for _ in 0..4000 {
            let code = recovery_code();
            assert_eq!(code.len(), 9, "{code}");
            for (i, c) in code.chars().enumerate() {
                if i == 4 {
                    assert_eq!(c, '-', "{code}");
                } else {
                    assert!(ALPHABET.contains(c), "{code}");
                    seen[i].insert(c);
                }
            }
        }
        for (i, s) in seen.iter().enumerate() {
            if i != 4 {
                // 4000 uniform draws miss a symbol with negligible odds;
                // a 16-value position can never exceed 16.
                assert!(s.len() > 16, "position {i} saw only {} values", s.len());
            }
        }
    }
}
