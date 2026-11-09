use crate::error::{AppError, Result};

use crate::query::ast::{Expr, Modifiers, Predicate, PredicateKind};

use super::lexer::Token;
use super::modifiers::{apply_modifier, is_comparison_kw, modifier_from_kw};
use super::predicate::build_predicate;

// ---------------------------------------------------------------------------
// Parser state
// ---------------------------------------------------------------------------

pub(super) struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub(super) fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn at_group_end(&self) -> bool {
        matches!(self.peek(), Token::Eof | Token::RAngle)
    }

    // or_expr = and_expr ("|" and_expr)*
    pub(super) fn parse_or(&mut self) -> Result<Expr> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Token::Pipe) {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    // and_expr = not_expr (not_expr)* - space is AND
    fn parse_and(&mut self) -> Result<Expr> {
        let mut left = self.parse_not()?;
        while !self.at_group_end() && !matches!(self.peek(), Token::Pipe) {
            let right = self.parse_not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    // not_expr = "!" atom | atom
    fn parse_not(&mut self) -> Result<Expr> {
        if matches!(self.peek(), Token::Bang) {
            self.advance();
            let inner = self.parse_atom()?;
            Ok(Expr::Not(Box::new(inner)))
        } else {
            self.parse_atom()
        }
    }

    // atom = "<" expr ">" | quoted_phrase | term
    fn parse_atom(&mut self) -> Result<Expr> {
        match self.peek() {
            Token::LAngle => {
                self.advance();
                let inner = self.parse_or()?;
                if matches!(self.peek(), Token::RAngle) {
                    self.advance();
                }
                Ok(inner)
            }
            Token::QuotedPhrase(_) => {
                if let Token::QuotedPhrase(s) = self.advance() {
                    Ok(Expr::Predicate(Predicate {
                        kind: PredicateKind::Term(s),
                        modifiers: Modifiers {
                            whole_filename: Some(true),
                            ..Default::default()
                        },
                    }))
                } else {
                    unreachable!()
                }
            }
            Token::Eof => Err(AppError::Query("unexpected end of query".into())),
            _ => self.parse_term(),
        }
    }

    // term = modifier* (function | word)
    // Modifiers are words followed by ':' that map to known modifier keywords.
    fn parse_term(&mut self) -> Result<Expr> {
        let mut mods = Modifiers::default();

        loop {
            let tok = match self.peek().clone() {
                Token::Word(w) => w,
                other => {
                    return Err(AppError::Query(format!("expected term, got {other:?}")));
                }
            };
            self.advance();

            // Split on first colon
            if let Some(colon) = tok.find(':') {
                let kw = tok[..colon].to_ascii_lowercase();
                let raw_val = tok[colon + 1..].to_owned();

                // Modifier keyword - accumulate and loop
                if let Some(m) = modifier_from_kw(&kw, &raw_val) {
                    apply_modifier(&mut mods, m);
                    continue;
                }

                // Comparison-capable functions may have the operator tokenised
                // separately: "size:>1mb" → Word("size:") + RAngle + Word("1mb").
                let val = if raw_val.is_empty() && is_comparison_kw(&kw) {
                    absorb_cmp_op(self)
                } else {
                    raw_val
                };

                // Function or type macro - build expression and return
                return build_predicate(&kw, &val, mods);
            }

            // Plain word - Term predicate
            return Ok(Expr::Predicate(Predicate {
                kind: PredicateKind::Term(tok),
                modifiers: mods,
            }));
        }
    }
}

// ---------------------------------------------------------------------------
// Comparison operator absorption helpers
// ---------------------------------------------------------------------------

/// Consume a split comparison operator token (RAngle/LAngle) optionally
/// followed by the rest of the value word. Returns the reconstructed string.
fn absorb_cmp_op(parser: &mut Parser) -> String {
    let op = match parser.peek().clone() {
        Token::RAngle => {
            parser.advance();
            '>'
        }
        Token::LAngle => {
            parser.advance();
            '<'
        }
        _ => return String::new(),
    };
    let rest = if let Token::Word(w) = parser.peek().clone() {
        parser.advance();
        w
    } else {
        String::new()
    };
    format!("{op}{rest}")
}
