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

/// LRC: sum of hex-decoded bytes, then two's complement, masked to 8 bits.
/// Input is a hex string (pairs of hex chars). Returns LRC as a hex string (2 chars uppercase).
pub fn lrc(hex_data: &str) -> Option<String> {
    let bytes = hex_decode(hex_data)?;
    let sum: u8 = bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    let lrc_val = (!sum).wrapping_add(1);
    Some(format!("{:02X}", lrc_val))
}

/// XOR8: XOR of all bytes, masked to 8 bits.
pub fn xor8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |acc, &b| acc ^ b)
}

/// Decode hex string into bytes. Returns None if invalid.
fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    let hex = hex.as_bytes();
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut result = Vec::with_capacity(hex.len() / 2);
    for chunk in hex.chunks(2) {
        let hi = hex_val(chunk[0])?;
        let lo = hex_val(chunk[1])?;
        result.push((hi << 4) | lo);
    }
    Some(result)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
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
        // Example: bytes 0x01, 0x02, 0x03 -> sum=0x06, LRC=0xFA
        let result = lrc("010203").unwrap();
        assert_eq!(result, "FA");
    }

    #[test]
    fn test_lrc_invalid() {
        assert!(lrc("0G").is_none());
        assert!(lrc("0").is_none());
    }
}
