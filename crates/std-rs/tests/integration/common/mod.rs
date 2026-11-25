/// `text` with every address replaced by `%p`. `(nil)` stays: which slots
/// are NULL is the dset's shape, not an address.
pub fn masked(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("0x") {
        out.push_str(&rest[..i]);
        let hex = rest[i + 2..]
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(rest.len() - i - 2);
        out.push_str("%p");
        rest = &rest[i + 2 + hex..];
    }
    out.push_str(rest);
    out
}
