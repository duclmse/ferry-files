use crate::error::{AppError, Result};
use crate::store::{CmpOp, DupeKind, Sort, SortKey};

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
            kind: PredicateKind::TypeMacro(tm),
            modifiers: mods,
        }));
    }

    // Windows drive letter prefix: single alpha char (e.g. c:, d:)
    if kw.len() == 1 && kw.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        let path_val = format!("{kw}:{val}");
        return Ok(Expr::Predicate(Predicate {
            kind: PredicateKind::Function(Function::Path(path_val)),
            modifiers: Modifiers {
                match_path: Some(true),
                ..mods
            },
        }));
    }

    let func = match kw {
        "dm" | "datemodified" | "date" => Function::DateModified(parse_date_value(val)?),
        "dc" | "datecreated" => Function::DateCreated(parse_date_value(val)?),
        "da" | "dateaccessed" => Function::DateAccessed(parse_date_value(val)?),
        "size" => Function::Size(parse_size_value(val)?),
        "ext" | "type" => {
            let exts: Vec<String> = val
                .split(';')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_ascii_lowercase())
                .collect();
            Function::Ext(exts)
        }
        "parent" | "dir" | "infolder" | "nosubfolders" => Function::Parent(val.to_owned()),
        "content" | "utf8content" | "ansicontent" => Function::Content(val.to_owned()),
        "root" if val.is_empty() => Function::Depth(CmpOp::Eq, 1),
        // Documented for Everything compatibility but not backed by the index;
        // an error beats treating `rc:today` as text and silently finding nothing.
        "rc" | "recentchange" | "dr" | "daterun" | "childcount" | "childfilecount"
        | "childfoldercount" | "empty" | "namepartdupe" | "dadupe" | "attribdupe" => {
            return Err(AppError::Query(format!("`{kw}:` is not supported")));
        }
        "fuzzy" => {
            return Ok(Expr::Predicate(Predicate {
                kind: PredicateKind::Fuzzy(val.to_owned()),
                modifiers: mods,
            }));
        }
        "regex" => {
            return Ok(Expr::Predicate(Predicate {
                kind: PredicateKind::Regex(val.to_owned()),
                modifiers: mods,
            }));
        }
        "dupe" => Function::Dupe(match val.to_ascii_lowercase().as_str() {
            "" | "name" => DupeKind::Name,
            "size" => DupeKind::Size,
            "dm" | "date" | "datemodified" => DupeKind::DateModified,
            "dc" | "datecreated" => DupeKind::DateCreated,
            // Read as a name-dupe search, anything else would quietly answer
            // a different question.
            _ => {
                return Err(AppError::Query(format!(
                    "`dupe:{val}`: the property must be name, size, dm or dc; \
                     narrow with separate terms (`dupe:size *.mp4`)"
                )))
            }
        }),
        "namedup" | "namedupe" | "sizedupe" | "sizedup" | "dmdupe" | "dmdup" | "dcdupe"
        | "dcdup"
            if !val.is_empty() =>
        {
            return Err(AppError::Query(format!(
                "`{kw}:` takes no value; narrow with separate terms (`{kw}: *.mp4`)"
            )));
        }
        "namedup" | "namedupe" => Function::Dupe(DupeKind::Name),
        "sizedupe" | "sizedup" => Function::Dupe(DupeKind::Size),
        "dmdupe" | "dmdup" => Function::Dupe(DupeKind::DateModified),
        "dcdupe" | "dcdup" => Function::Dupe(DupeKind::DateCreated),
        "count" => {
            let n = val
                .parse::<usize>()
                .map_err(|_| AppError::Query(format!("invalid count value: '{val}'")))?;
            Function::Count(n)
        }
        "sort" => Function::Sort(parse_sort(val)?),
        "len" | "length" => return count_predicate(val, mods, Function::Len),
        "depth" | "parents" => return count_predicate(val, mods, Function::Depth),
        "attrib" => Function::Attrib(attrib_mask(val)?),
        "child" => Function::Child(val.to_owned()),
        "startwith" | "startswith" => Function::StartWith(val.to_owned()),
        "endwith" | "endswith" => Function::EndWith(val.to_owned()),
        "archiveof" => Function::ArchiveOf(val.to_owned()),
        _ => {
            // Unknown keyword - treat "keyword:value" as a literal term
            return Ok(Expr::Predicate(Predicate {
                kind: PredicateKind::Term(format!("{kw}:{val}")),
                modifiers: mods,
            }));
        }
    };

    Ok(Expr::Predicate(Predicate {
        kind: PredicateKind::Function(func),
        modifiers: mods,
    }))
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// `sort:name`, `sort:size-asc`, `sort:dm-descending`. With no direction,
/// names run A to Z and sizes and dates largest and newest first.
fn parse_sort(val: &str) -> Result<Sort> {
    let val = val.to_ascii_lowercase();
    let (field, direction) = match val.rsplit_once('-') {
        Some((f, d @ ("asc" | "ascending" | "desc" | "descending"))) => (f, Some(d)),
        _ => (val.as_str(), None),
    };
    let key = match field {
        "name" => SortKey::Name,
        "size" => SortKey::Size,
        "dm" | "date" | "datemodified" | "date-modified" => SortKey::DateModified,
        "dc" | "datecreated" | "date-created" => SortKey::DateCreated,
        _ => return Err(AppError::Query(format!("invalid sort value: '{val}'"))),
    };
    let descending = match direction {
        Some(d) => d.starts_with("desc"),
        None => key != SortKey::Name,
    };
    Ok(Sort { key, descending })
}

/// `len:`/`depth:` value: a comparison, or an inclusive `a..b` / `a-b` range
/// built as two comparisons.
fn count_predicate(val: &str, mods: Modifiers, make: fn(CmpOp, usize) -> Function) -> Result<Expr> {
    let pred = |f: Function| {
        Expr::Predicate(Predicate {
            kind: PredicateKind::Function(f),
            modifiers: mods.clone(),
        })
    };
    let range = val.split_once("..").or_else(|| val.split_once('-'));
    if let Some((a, b)) = range {
        let num = |s: &str| {
            s.trim()
                .parse::<usize>()
                .map_err(|_| AppError::Query(format!("invalid number range: '{val}'")))
        };
        return Ok(Expr::And(
            Box::new(pred(make(CmpOp::Gte, num(a)?))),
            Box::new(pred(make(CmpOp::Lte, num(b)?))),
        ));
    }
    let (op, n) = parse_cmp_usize(val)?;
    Ok(pred(make(op, n)))
}

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
    let n = rest
        .trim()
        .parse::<usize>()
        .map_err(|_| AppError::Query(format!("invalid number: '{s}'")))?;
    Ok((op, n))
}

fn attrib_mask(s: &str) -> Result<u32> {
    use crate::store::{
        ATTRIB_ARCHIVE, ATTRIB_COMPRESSED, ATTRIB_DIRECTORY, ATTRIB_ENCRYPTED, ATTRIB_HIDDEN,
        ATTRIB_NORMAL, ATTRIB_OFFLINE, ATTRIB_READONLY, ATTRIB_REPARSE_POINT, ATTRIB_SPARSE,
        ATTRIB_SYSTEM, ATTRIB_TEMPORARY,
    };
    let mut mask = 0u32;
    for c in s.chars() {
        mask |= match c.to_ascii_uppercase() {
            'A' => ATTRIB_ARCHIVE,
            'C' => ATTRIB_COMPRESSED,
            'D' => ATTRIB_DIRECTORY,
            'E' => ATTRIB_ENCRYPTED,
            'H' => ATTRIB_HIDDEN,
            'J' | 'L' => ATTRIB_REPARSE_POINT,
            'N' => ATTRIB_NORMAL,
            'O' => ATTRIB_OFFLINE,
            'P' => ATTRIB_SPARSE,
            'R' => ATTRIB_READONLY,
            'S' => ATTRIB_SYSTEM,
            'T' => ATTRIB_TEMPORARY,
            // Matching nothing beats matching everything with a zero mask.
            _ => {
                return Err(AppError::Query(format!(
                    "unknown attribute '{c}' in attrib:{s}"
                )))
            }
        };
    }
    Ok(mask)
}
