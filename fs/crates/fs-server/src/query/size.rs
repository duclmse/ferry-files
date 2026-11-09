use super::ast::RangeExpr;
use crate::error::{AppError, Result};

pub fn parse_size_value(s: &str) -> Result<RangeExpr<u64>> {
    let s = s.trim();
    match s.to_ascii_lowercase().as_str() {
        "empty" => return Ok(RangeExpr::Eq(0)),
        "tiny" => return Ok(RangeExpr::Between(1, 10 * 1024)),
        "small" => return Ok(RangeExpr::Between(10 * 1024 + 1, 100 * 1024)),
        "medium" => return Ok(RangeExpr::Between(100 * 1024 + 1, 1024 * 1024)),
        "large" => return Ok(RangeExpr::Between(1024 * 1024 + 1, 16 * 1024 * 1024)),
        "huge" => return Ok(RangeExpr::Between(16 * 1024 * 1024 + 1, 128 * 1024 * 1024)),
        "gigantic" => return Ok(RangeExpr::Gte(128 * 1024 * 1024 + 1)),
        _ => {}
    }
    // Range: 1mb..10mb
    if let Some(pos) = s.find("..") {
        let lo = parse_bytes(&s[..pos])?;
        let hi = parse_bytes(&s[pos + 2..])?;
        return Ok(RangeExpr::Between(lo, hi));
    }
    // Comparison prefixes
    if let Some(rest) = s.strip_prefix("<=") {
        return Ok(RangeExpr::Lte(parse_bytes(rest)?));
    }
    if let Some(rest) = s.strip_prefix(">=") {
        return Ok(RangeExpr::Gte(parse_bytes(rest)?));
    }
    if let Some(rest) = s.strip_prefix('<') {
        return Ok(RangeExpr::Lt(parse_bytes(rest)?));
    }
    if let Some(rest) = s.strip_prefix('>') {
        return Ok(RangeExpr::Gt(parse_bytes(rest)?));
    }
    if let Some(rest) = s.strip_prefix('=') {
        return Ok(RangeExpr::Eq(parse_bytes(rest)?));
    }
    Ok(RangeExpr::Eq(parse_bytes(s)?))
}

fn parse_bytes(s: &str) -> Result<u64> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    let (num_str, mult) = if lower.ends_with("kb") {
        (&s[..s.len() - 2], 1024u64)
    } else if lower.ends_with("mb") {
        (&s[..s.len() - 2], 1024 * 1024)
    } else if lower.ends_with("gb") {
        (&s[..s.len() - 2], 1024 * 1024 * 1024)
    } else if lower.ends_with('k') {
        (&s[..s.len() - 1], 1024)
    } else if lower.ends_with('m') {
        (&s[..s.len() - 1], 1024 * 1024)
    } else if lower.ends_with('g') {
        (&s[..s.len() - 1], 1024 * 1024 * 1024)
    } else {
        (s, 1)
    };
    let n: f64 = num_str
        .trim()
        .parse()
        .map_err(|_| AppError::Query(format!("invalid size value: '{s}'")))?;
    Ok((n * mult as f64) as u64)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_large() {
        let r = parse_size_value("large").unwrap();
        assert!(matches!(r, RangeExpr::Between(lo, hi) if lo == 1024*1024+1 && hi == 16*1024*1024));
    }

    #[test]
    fn gt_mb() {
        let r = parse_size_value(">1mb").unwrap();
        assert!(matches!(r, RangeExpr::Gt(v) if v == 1024*1024));
    }

    #[test]
    fn range_mb() {
        let r = parse_size_value("2mb..10mb").unwrap();
        assert!(matches!(r, RangeExpr::Between(lo, hi)
            if lo == 2*1024*1024 && hi == 10*1024*1024));
    }

    #[test]
    fn gigantic_lower_bound() {
        let r = parse_size_value("gigantic").unwrap();
        assert!(matches!(r, RangeExpr::Gte(v) if v == 128*1024*1024 + 1));
    }
}
