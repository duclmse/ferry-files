use crate::error::{AppError, Result};
use crate::store::{CmpOp, DupeKind};

use crate::query::ast::{Expr, Function, Modifiers, Predicate, PredicateKind};
use crate::query::date::parse_date_value;
use crate::query::macros::keyword_to_type_macro;
use crate::query::size::parse_size_value;

// ---------------------------------------------------------------------------
// Build an Expr from a recognised keyword:value token
// ---------------------------------------------------------------------------

pub(super) fn build_predicate(kw: &str, val: &str, mods: Modifiers) -> Result<Expr> {
    // Type macros
    if let Some(tm) = keyword_to_type_macro(kw) {
        return Ok(Expr::Predicate(Predicate {
            kind:      PredicateKind::TypeMacro(tm),
            modifiers: mods,
        }));
    }

    // Windows drive letter prefix: single alpha char (e.g. c:, d:)
    if kw.len() == 1 && kw.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        let path_val = format!("{kw}:{val}");
        return Ok(Expr::Predicate(Predicate {
            kind:      PredicateKind::Function(Function::Path(path_val)),
            modifiers: Modifiers { match_path: Some(true), ..mods },
        }));
    }

    let func = match kw {
        "dm" | "datemodified" | "date" => {
            Function::DateModified(parse_date_value(val)?)
        }
        "dc" | "datecreated" => {
            Function::DateCreated(parse_date_value(val)?)
        }
        "da" | "dateaccessed" => {
            Function::DateAccessed(parse_date_value(val)?)
        }
        "size" => {
            Function::Size(parse_size_value(val)?)
        }
        "ext" | "type" => {
            let exts: Vec<String> = val
                .split(';')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_ascii_lowercase())
                .collect();
            Function::Ext(exts)
        }
        "path" => Function::Path(val.to_owned()),
        "parent" | "dir" => Function::Parent(val.to_owned()),
        "content" => Function::Content(val.to_owned()),
        "fuzzy" => {
            return Ok(Expr::Predicate(Predicate {
                kind:      PredicateKind::Fuzzy(val.to_owned()),
                modifiers: mods,
            }));
        }
        "regex" => {
            return Ok(Expr::Predicate(Predicate {
                kind:      PredicateKind::Regex(val.to_owned()),
                modifiers: mods,
            }));
        }
        "dupe" | "namedup" => Function::Dupe(DupeKind::Name),
        "sizedupe" => Function::Dupe(DupeKind::Size),
        "dmdup"    => Function::Dupe(DupeKind::DateModified),
        "dcdup"    => Function::Dupe(DupeKind::DateCreated),
        "count" => {
            let n = val.parse::<usize>()
                .map_err(|_| AppError::Query(format!("invalid count value: '{val}'")))?;
            Function::Count(n)
        }
        "len" | "length" => {
            let (op, n) = parse_cmp_usize(val)?;
            Function::Len(op, n)
        }
        "depth" => {
            let (op, n) = parse_cmp_usize(val)?;
            Function::Depth(op, n)
        }
        "attrib" => {
            let mask = attrib_mask(val);
            Function::Attrib(mask)
        }
        "child" => Function::Child(val.to_owned()),
        "startwith" | "startswith" => Function::StartWith(val.to_owned()),
        "endwith"   | "endswith"   => Function::EndWith(val.to_owned()),
        "archiveof" => Function::ArchiveOf(val.to_owned()),
        _ => {
            // Unknown keyword — treat "keyword:value" as a literal term
            return Ok(Expr::Predicate(Predicate {
                kind:      PredicateKind::Term(format!("{kw}:{val}")),
                modifiers: mods,
            }));
        }
    };

    Ok(Expr::Predicate(Predicate {
        kind:      PredicateKind::Function(func),
        modifiers: mods,
    }))
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn parse_cmp_usize(s: &str) -> Result<(CmpOp, usize)> {
    let (op, rest) = if let Some(r) = s.strip_prefix("<=") {
        (CmpOp::Lte, r)
    } else if let Some(r) = s.strip_prefix(">=") {
        (CmpOp::Gte, r)
    } else if let Some(r) = s.strip_prefix('<') {
        (CmpOp::Lt, r)
    } else if let Some(r) = s.strip_prefix('>') {
        (CmpOp::Gt, r)
    } else if let Some(r) = s.strip_prefix('=') {
        (CmpOp::Eq, r)
    } else {
        (CmpOp::Eq, s)
    };
    let n = rest.trim().parse::<usize>()
        .map_err(|_| AppError::Query(format!("invalid number: '{s}'")))?;
    Ok((op, n))
}

fn attrib_mask(s: &str) -> u32 {
    use crate::store::{
        ATTRIB_ARCHIVE, ATTRIB_COMPRESSED, ATTRIB_DIRECTORY, ATTRIB_ENCRYPTED,
        ATTRIB_HIDDEN, ATTRIB_NORMAL, ATTRIB_OFFLINE, ATTRIB_READONLY,
        ATTRIB_REPARSE_POINT, ATTRIB_SPARSE, ATTRIB_SYSTEM, ATTRIB_TEMPORARY,
    };
    let mut mask = 0u32;
    for c in s.chars() {
        mask |= match c.to_ascii_uppercase() {
            'A' => ATTRIB_ARCHIVE,
            'C' => ATTRIB_COMPRESSED,
            'D' => ATTRIB_DIRECTORY,
            'E' => ATTRIB_ENCRYPTED,
            'H' => ATTRIB_HIDDEN,
            'J' => ATTRIB_REPARSE_POINT,
            'N' => ATTRIB_NORMAL,
            'O' => ATTRIB_OFFLINE,
            'P' => ATTRIB_SPARSE,
            'R' => ATTRIB_READONLY,
            'S' => ATTRIB_SYSTEM,
            'T' => ATTRIB_TEMPORARY,
            _ => 0,
        };
    }
    mask
}
