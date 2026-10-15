//! Text from outside shown on one line: each control character and each mark that reorders text or separates lines
//! escaped ([`escaped`]), so a value cannot break or forge a line, nor change how the rest of it reads.

/// `text` with each control character escaped, and each mark that reorders text or separates lines, so a value cannot
/// break or forge a line, nor change how the rest of it reads. Escaping twice changes nothing more.
#[must_use]
pub fn escaped(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                character.escape_debug().to_string()
            } else if is_reordering_mark(character) {
                format!("\\u{{{:x}}}", u32::from(character))
            } else {
                character.to_string()
            }
        })
        .collect()
}

/// Whether `character` is a bidirectional mark, embedding, override or isolate, or the line or paragraph separator:
/// U+061C, U+200E, U+200F, U+202A to U+202E, U+2066 to U+2069, U+2028 and U+2029.
#[must_use]
pub fn is_reordering_mark(character: char) -> bool {
    matches!(
        character,
        '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{2028}' | '\u{2029}'
    )
}

#[cfg(test)]
mod tests {
    use super::escaped;

    #[test]
    fn the_bidirectional_and_line_separator_marks_are_escaped_as_control_characters_are() {
        let marks = "\u{61c}\u{200e}\u{200f}\u{202a}\u{202b}\u{202c}\u{202d}\u{202e}\u{2066}\u{2067}\u{2068}\u{2069}\u{2028}\u{2029}";
        assert_eq!(
            escaped(&format!("a{marks}b\u{1b}\r")),
            r"a\u{61c}\u{200e}\u{200f}\u{202a}\u{202b}\u{202c}\u{202d}\u{202e}\u{2066}\u{2067}\u{2068}\u{2069}\u{2028}\u{2029}b\u{1b}\r"
        );
        assert_eq!(
            escaped("\u{200d}\u{2065}\u{206a}\u{e9}"),
            "\u{200d}\u{2065}\u{206a}\u{e9}",
            "the marks around them stay"
        );
        assert_eq!(
            escaped(&escaped("\u{202e}\u{1b}")),
            escaped("\u{202e}\u{1b}"),
            "escaping twice changes nothing"
        );
    }
}
