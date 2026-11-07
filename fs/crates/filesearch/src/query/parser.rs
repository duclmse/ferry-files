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
    // Expand character escape macros
    let expanded = raw.replace("quot:", "\"").replace("apos:", "'");

    // Expand user-defined macros (single pass, no recursion)
    let expanded = expand_user_macros(&expanded, macros);

    let tokens = tokenize(&expanded);
    let mut parser = Parser::new(tokens);
    let expr = parser.parse_or()?;
    Ok(expr)
}

fn expand_user_macros(s: &str, macros: &MacroConfig) -> String {
    if macros.entries.is_empty() {
        return s.to_owned();
    }
    let mut result = s.to_owned();
    for (name, replacement) in &macros.entries {
        let pattern = format!("{name}:");
        result = result.replace(&pattern, &format!("{replacement} "));
    }
    result
}
