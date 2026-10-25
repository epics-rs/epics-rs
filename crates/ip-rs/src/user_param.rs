//! The `KEY=value` userParam syntax `devXxStrParm.c` and `devXxEurotherm.c`
//! share, each key found with `strstr`.

/// C `strstr(userParam, key)`, and what follows the key.
pub fn after<'a>(user_param: &'a str, key: &str) -> Option<&'a str> {
    user_param.find(key).map(|at| &user_param[at + key.len()..])
}

/// `for (i=0; i<31 && *p && *p != ','; ...)`: a value up to its comma.
pub fn up_to_comma(value: &str) -> String {
    let mut end = value.find(',').unwrap_or(value.len()).min(31);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// `TERM=`'s `HEXCHAR2INT` pairs: two hex digits a byte, at most nine.
pub fn term(hex: &str) -> Vec<u8> {
    let digit = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
    hex.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .take_while(|pair| pair.iter().all(u8::is_ascii_hexdigit))
        .take(9)
        .map(|&[high, low]| digit(high) * 16 + digit(low))
        .collect()
}

/// A C string: the bytes up to the first NUL.
pub fn c_str(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_values_and_terminators_read_as_c_does() {
        assert_eq!(after("TERM=,IX=3", "IX="), Some("3"));
        assert_eq!(after("IX=3", "TO="), None);
        assert_eq!(up_to_comma("%lf,N=10"), "%lf");
        assert_eq!(up_to_comma(&"x".repeat(40)).len(), 31);
        assert_eq!(term("0d0a3"), b"\r\n");
        assert_eq!(term(",IX=3"), b"");
        assert_eq!(c_str(b"ab\0cd"), b"ab");
    }
}
