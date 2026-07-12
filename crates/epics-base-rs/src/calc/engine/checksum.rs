/// CRC-16 with polynomial 0xA001 (MODBUS).
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
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
