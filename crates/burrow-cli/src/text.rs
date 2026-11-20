//! Text from elsewhere, made safe to print.

/// `text` with control characters escaped, for names and messages that
/// came from a server or a config file someone else may have written.
pub fn clean(text: &str) -> String {
    text.chars()
        .flat_map(|c| -> Box<dyn Iterator<Item = char>> {
            if c.is_control() {
                Box::new(c.escape_unicode())
            } else {
                Box::new(std::iter::once(c))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_lose_their_control_characters() {
        assert_eq!(clean("web\u{1b}]0;owned\u{7}"), "web\\u{1b}]0;owned\\u{7}");
        assert_eq!(clean("plain"), "plain");
    }
}
