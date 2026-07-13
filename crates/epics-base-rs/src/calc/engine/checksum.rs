/// C `crc16` (`sCalcPerform.c:192-229`) — polynomial 0xA001, seed 0xFFFF.
///
/// **This deliberately DEVIATES from the CRC-16/MODBUS standard for any byte
/// ≥ 0x80, because compiled C does.** Reproducing C is the whole point of the
/// operator: a `CRC16` frame exists to be accepted by a device that is already
/// talking to a C IOC, so wire-compatibility with C beats agreement with the
/// specification. On an ASCII-only payload the two coincide.
///
/// The deviation is C's, and it is unintentional:
///
/// ```c
/// unsigned int crc;                  /* 32 bits, NOT 16 */
/// char tranInput[SCALC_STRING_SIZE]; /* `char` — SIGNED on x86-64/ARM64 Linux */
/// crc = 0xffff;
/// for (i=0; i<len; i++) {
///     /* crc = (crc&0x0ff00) | ((crc&0x0ff) ^ (unsigned int)tranInput[i]); */
///     crc ^= (unsigned int)tranInput[i];
///     for (j=0; j<8; j++) { ... crc >>= 1; crc ^= 0x0A001; ... }
/// }
/// sprintf(output, "\\x%02x\\x%02x", crc&0xff, (crc&0xff00)>>8);
/// ```
///
/// `(unsigned int)tranInput[i]` sign-extends a byte ≥ 0x80 to `0xFFFFFF80..`,
/// so the XOR pollutes bits 16-31 of a 32-bit accumulator that is never masked
/// back to 16. The eight `crc >>= 1` steps then shift that garbage down into
/// the CRC proper, and it survives into the next byte. The commented-out line
/// directly above (`:211`) is the author's own masked version — evidence the
/// standard behaviour was intended and the cast was the slip.
///
/// So the accumulator here is `u32` and the byte is sign-extended, exactly as C
/// does it; only the final two bytes are taken. Compiled sCalc (`gcc -O0`,
/// x86-64): `CRC16("\x80")` = `\x41\x1f`, `CRC16("\xff")` = `\x00\xff`,
/// `CRC16("\x80\x80")` = `\x21\x90`, `CRC16("123456789")` = `\x37\x4b` (the
/// standard 0x4B37 — ASCII agrees).
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u32 = 0xFFFF;
    for &byte in data {
        // C `(unsigned int)(char)byte` — sign-extend through the platform's
        // signed `char`, then reinterpret. This is the bug being reproduced.
        crc ^= (byte as i8) as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    // C's `sprintf` takes `crc&0xff` and `(crc&0xff00)>>8` — the high garbage
    // above bit 15 never reaches the wire, only the value below it.
    crc as u16
}

/// C `lrc` (`sCalcPerform.c:242-256`) — the ASCII-MODBUS longitudinal
/// redundancy check. The operand is ASCII HEX PAIRS (no escape translation,
/// unlike [`crc16`]'s and [`xor8`]'s operands), and the result is two UPPERCASE
/// hex characters, unescaped.
///
/// ```c
/// for (i=0, lrc=0; i<strlen(rawInput)-1; i+=2)
///     lrc += hex(rawInput[i])*0x10 + hex(rawInput[i+1]);
/// lrc &= 0xff;  lrc = -lrc;
/// sprintf(output, "%02X", lrc&0xff);
/// return(0);            /* never fails */
/// ```
///
/// C rejects nothing, so neither does this. Two consequences the port used to
/// get wrong by refusing the operand outright (which, since a failed helper
/// makes the opcode a no-op, silently left the frame unchecksummed):
///
///   * `hex()` (`:232`) answers **0** for a character that is not a hex digit,
///     so `LRC("0G")` is a byte 0x00 and the check is "00" — not an error.
///   * the loop steps in pairs and stops at `strlen-1`, so a trailing ODD
///     character is ignored: `LRC("010")` is `LRC("01")` is "FF".
///
/// Compiled sCalc: `LRC("010203")`="FA", `LRC("F7031389000A")`="60",
/// `LRC("010")`="FF", `LRC("0G")`="00".
///
/// `None` is for the EMPTY operand alone, and it is NOT what C does: for `""`,
/// `strlen(rawInput)-1` wraps (it is `size_t`), the loop runs, and C reads two
/// bytes past the end of a zero-length string. That is undefined behaviour —
/// the harness segfaults on it — so it is refused here rather than reproduced.
pub fn lrc(hex_data: &[u8]) -> Option<String> {
    if hex_data.is_empty() {
        return None;
    }
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < hex_data.len() {
        sum += (hex_val(hex_data[i]) as u32) * 0x10 + hex_val(hex_data[i + 1]) as u32;
        i += 2;
    }
    Some(format!("{:02X}", ((sum & 0xff) as u8).wrapping_neg()))
}

/// XOR8: XOR of all bytes, masked to 8 bits.
pub fn xor8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |acc, &b| acc ^ b)
}

/// C `hex` (`sCalcPerform.c:232-240`): the digit's value, or **0** for anything
/// that is not a hex digit. It has no failure mode.
fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc16_empty() {
        assert_eq!(crc16(&[]), 0xFFFF);
    }

    #[test]
    fn test_crc16_known() {
        // "123456789" -> 0x4B37 (standard MODBUS CRC-16)
        let data = b"123456789";
        assert_eq!(crc16(data), 0x4B37);
    }

    /// Compiled sCalc (`gcc -O0`, x86-64) — a byte ≥ 0x80 is sign-extended into
    /// a 32-bit accumulator, so the digest is NOT the standard one. See
    /// [`crc16`]'s doc: reproducing C is deliberate.
    #[test]
    fn test_crc16_sign_extends_high_bytes_like_c() {
        // Standard CRC-16/MODBUS of a lone 0x80 is 0xE0BE; C says 0x1F41.
        assert_eq!(crc16(&[0x80]), 0x1F41);
        assert_eq!(crc16(&[0xFF]), 0xFF00);
        // The pollution above bit 15 survives into the next byte.
        assert_eq!(crc16(&[0x80, 0x80]), 0x9021);
        assert_eq!(crc16(&[0xF7, 0x03, 0x13, 0x89]), 0xB9C0);
        assert_eq!(crc16(&[0xC3, 0xA9]), 0x7ED1);
        // One high byte after an ASCII one — the mixed frame.
        assert_eq!(crc16(&[b'A', 0xC3]), 0x4E8E);
    }

    #[test]
    fn test_xor8() {
        assert_eq!(xor8(&[0x01, 0x02, 0x03]), 0x00);
        assert_eq!(xor8(&[0xFF, 0x00]), 0xFF);
    }

    #[test]
    fn test_lrc() {
        // Compiled sCalc, `drv 'LRC("...")'`.
        assert_eq!(lrc(b"010203").unwrap(), "FA");
        assert_eq!(lrc(b"F7031389000A").unwrap(), "60");
    }

    /// C's `hex()` answers 0 for a non-hex character and its loop ignores a
    /// trailing odd one, so neither is an error. Compiled sCalc: "00" and "FF".
    #[test]
    fn test_lrc_accepts_what_c_accepts() {
        assert_eq!(lrc(b"0G").unwrap(), "00");
        assert_eq!(lrc(b"010").unwrap(), "FF");
        assert_eq!(lrc(b"01").unwrap(), "FF");
        // A trailing odd character is ignored, so "0" is an empty pair list.
        assert_eq!(lrc(b"0").unwrap(), "00");
    }

    /// The one operand C does NOT handle: `strlen("")-1` wraps and C reads out
    /// of bounds. Refused deliberately — see [`lrc`].
    #[test]
    fn test_lrc_refuses_the_empty_operand() {
        assert!(lrc(b"").is_none());
    }
}
