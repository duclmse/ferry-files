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
            c if c.is_whitespace() => { i += 1; }
            '|' => { tokens.push(Token::Pipe);   i += 1; }
            '!' => { tokens.push(Token::Bang);   i += 1; }
            '<' => { tokens.push(Token::LAngle); i += 1; }
            '>' => { tokens.push(Token::RAngle); i += 1; }
            '"' => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '"' { i += 1; }
                let s: String = chars[start..i].iter().collect();
                tokens.push(Token::QuotedPhrase(s));
                if i < chars.len() { i += 1; }
            }
            _ => {
                let start = i;
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], '|' | '!' | '<' | '>')
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                tokens.push(Token::Word(word));
            }
        }
    }
    tokens.push(Token::Eof);
    tokens
}
