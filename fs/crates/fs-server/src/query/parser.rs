use crate::config::MacroConfig;
use crate::error::Result;

use super::ast::Expr;
#[cfg(test)]
use super::ast::{Function, PredicateKind};

mod grammar;
mod lexer;
mod modifiers;
mod predicate;

#[cfg(test)]
mod tests;

use grammar::Parser;
use lexer::tokenize;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Pre-process the query string (macro expansion) then parse into an AST.
pub fn parse(raw: &str, macros: &MacroConfig) -> Result<Expr> {
    let expanded = expand_user_macros(raw, macros);

    let tokens = tokenize(&expanded);
    let mut parser = Parser::new(tokens);
    let expr = parser.parse_or()?;
    Ok(expr)
}

/// Replaces `name:` with the macro's text where a token starts (not inside
/// a word such as `homework:`, and not inside quotes), in one pass so a
/// replacement is never expanded again.
fn expand_user_macros(s: &str, macros: &MacroConfig) -> String {
    if macros.entries.is_empty() {
        return s.to_owned();
    }
    // Longest name first, so `pics:` wins over `pic:` whatever the map order.
    let mut names: Vec<(&String, &String)> = macros.entries.iter().collect();
    names.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(b.0)));

    let mut out = String::with_capacity(s.len());
    let mut in_quote = false;
    let mut at_token_start = true;
    let mut i = 0;
    'outer: while let Some(c) = s[i..].chars().next() {
        if at_token_start && !in_quote {
            for (name, replacement) in &names {
                let end = i + name.len();
                if s.len() > end
                    && s.is_char_boundary(end)
                    && s[i..end].eq_ignore_ascii_case(name)
                    && s.as_bytes()[end] == b':'
                {
                    out.push_str(replacement);
                    out.push(' ');
                    i = end + 1;
                    continue 'outer;
                }
            }
        }
        if c == '"' {
            in_quote = !in_quote;
        }
        at_token_start = c.is_whitespace() || matches!(c, '|' | '!' | '<' | '>');
        out.push(c);
        i += c.len_utf8();
    }
    out
}
