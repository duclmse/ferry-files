// ---------------------------------------------------------------------------
// Tokeniser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Token {
    Word(String),
    QuotedPhrase(String),
    Pipe,
    Bang,
    LAngle,
    RAngle,
    Eof,
}

pub(super) fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            c if c.is_whitespace() => {
                i += 1;
            }
            '|' => {
                tokens.push(Token::Pipe);
                i += 1;
            }
            '!' => {
                tokens.push(Token::Bang);
                i += 1;
            }
            '<' => {
                tokens.push(Token::LAngle);
                i += 1;
            }
            '>' => {
                tokens.push(Token::RAngle);
                i += 1;
            }
            '"' => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '"' {
                    i += 1;
                }
                let s: String = chars[start..i].iter().collect();
                tokens.push(Token::QuotedPhrase(s));
                if i < chars.len() {
                    i += 1;
                }
            }
            _ => {
                let start = i;
                // `!` negates only at the start of a word; inside one it is
                // text, which keeps virtual paths like `a.zip!/b` whole.
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], '|' | '<' | '>')
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                tokens.push(Token::Word(unescape(&word)));
            }
        }
    }
    tokens.push(Token::Eof);
    tokens
}

/// Expands Everything's character escapes (`quot:`, `apos:`, `amp:`, `lt:`,
/// `gt:`, `#<decimal>:`, `#x<hex>:`). They run after tokenising so the
/// characters they produce are literal text, and only at a word boundary so
/// `result:` does not become `resu<`.
fn unescape(word: &str) -> String {
    const NAMED: [(&str, char); 5] = [
        ("quot:", '"'),
        ("apos:", '\''),
        ("amp:", '&'),
        ("lt:", '<'),
        ("gt:", '>'),
    ];
    let mut out = String::with_capacity(word.len());
    let mut rest = word;
    let mut at_boundary = true;
    'outer: while let Some(c) = rest.chars().next() {
        if at_boundary {
            for (name, ch) in NAMED {
                if rest
                    .get(..name.len())
                    .is_some_and(|p| p.eq_ignore_ascii_case(name))
                {
                    out.push(ch);
                    rest = &rest[name.len()..];
                    continue 'outer;
                }
            }
            if let Some((ch, used)) = codepoint_escape(rest) {
                out.push(ch);
                rest = &rest[used..];
                continue;
            }
        }
        out.push(c);
        at_boundary = !c.is_alphanumeric();
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// `#65:` or `#x41:` at the start of `s`: the character and bytes consumed.
fn codepoint_escape(s: &str) -> Option<(char, usize)> {
    let body = s.strip_prefix('#')?;
    let (digits, radix, skip) = match body.strip_prefix(['x', 'X']) {
        Some(hex) => (hex, 16, 2),
        None => (body, 10, 1),
    };
    let end = digits.find(':')?;
    let n = u32::from_str_radix(&digits[..end], radix).ok()?;
    Some((char::from_u32(n)?, skip + end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_expand_at_word_boundaries_only() {
        assert_eq!(unescape("quot:"), "\"");
        assert_eq!(unescape("quot:abcquot:"), "\"abcquot:");
        assert_eq!(unescape("quot:quot:"), "\"\"");
        assert_eq!(unescape("#65:#x42:c"), "ABc");
        assert_eq!(unescape("result:"), "result:");
        assert_eq!(unescape("case:lt:a"), "case:<a");
        assert_eq!(unescape("#:"), "#:");
        assert_eq!(unescape("#x110000:"), "#x110000:");
        assert_eq!(unescape("café"), "café");
        assert_eq!(unescape("é-#233:"), "é-é");
    }

    #[test]
    fn bang_inside_a_word_is_text() {
        assert_eq!(
            tokenize("!a b.zip!/c"),
            [
                Token::Bang,
                Token::Word("a".into()),
                Token::Word("b.zip!/c".into()),
                Token::Eof
            ]
        );
    }
}
