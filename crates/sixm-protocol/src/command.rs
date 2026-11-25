//! Command-line splitting shared by the server console and the client.

/// Split a command line into words. Double quotes group words and `\"` is a
/// literal quote inside them: `say "hello world"` gives `["say", "hello world"]`.
/// A leading `/` is ignored.
pub fn split_command(raw: &str) -> Vec<String> {
    let text = raw.trim();
    let text = text.strip_prefix('/').unwrap_or(text);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut has = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '\\' && chars.peek() == Some(&'"') {
                cur.push('"');
                chars.next();
            } else if c == '"' {
                quoted = false;
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            quoted = true;
            has = true;
        } else if c == ' ' || c == '\t' {
            if has || !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            has = false;
        } else {
            cur.push(c);
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::split_command;

    #[test]
    fn splits_words_and_quotes() {
        assert_eq!(split_command("say hello world"), ["say", "hello", "world"]);
        assert_eq!(
            split_command(r#"/say "hello world" x"#),
            ["say", "hello world", "x"]
        );
        assert_eq!(split_command(r#"a "b \"c\" d""#), ["a", r#"b "c" d"#]);
        assert_eq!(split_command(r#"set key """#), ["set", "key", ""]);
        assert_eq!(split_command("   \t "), Vec::<String>::new());
    }
}
