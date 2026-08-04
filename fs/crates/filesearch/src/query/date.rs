use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveDate, NaiveDateTime, TimeZone};

use crate::error::{AppError, Result};
use crate::store::CmpOp;
use super::ast::{DateConstant, DateExpr};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn parse_date_value(s: &str) -> Result<DateExpr> {
    let lower = s.trim().to_ascii_lowercase();

    // Named constants
    match lower.as_str() {
        "today"        => return Ok(DateExpr::Constant(DateConstant::Today)),
        "yesterday"    => return Ok(DateExpr::Constant(DateConstant::Yesterday)),
        "thisweek"     => return Ok(DateExpr::Constant(DateConstant::ThisWeek)),
        "lastweek"     => return Ok(DateExpr::Constant(DateConstant::LastWeek)),
        "thismonth"    => return Ok(DateExpr::Constant(DateConstant::ThisMonth)),
        "lastmonth"    => return Ok(DateExpr::Constant(DateConstant::LastMonth)),
        "thisyear"     => return Ok(DateExpr::Constant(DateConstant::ThisYear)),
        "lastyear"     => return Ok(DateExpr::Constant(DateConstant::LastYear)),
        "january"  | "jan" => return Ok(DateExpr::Constant(DateConstant::Month(1))),
        "february" | "feb" => return Ok(DateExpr::Constant(DateConstant::Month(2))),
        "march"    | "mar" => return Ok(DateExpr::Constant(DateConstant::Month(3))),
        "april"    | "apr" => return Ok(DateExpr::Constant(DateConstant::Month(4))),
        "may"              => return Ok(DateExpr::Constant(DateConstant::Month(5))),
        "june"     | "jun" => return Ok(DateExpr::Constant(DateConstant::Month(6))),
        "july"     | "jul" => return Ok(DateExpr::Constant(DateConstant::Month(7))),
        "august"   | "aug" => return Ok(DateExpr::Constant(DateConstant::Month(8))),
        "september"| "sep" => return Ok(DateExpr::Constant(DateConstant::Month(9))),
        "october"  | "oct" => return Ok(DateExpr::Constant(DateConstant::Month(10))),
        "november" | "nov" => return Ok(DateExpr::Constant(DateConstant::Month(11))),
        "december" | "dec" => return Ok(DateExpr::Constant(DateConstant::Month(12))),
        "sunday"   | "sun" => return Ok(DateExpr::Constant(DateConstant::Weekday(0))),
        "monday"   | "mon" => return Ok(DateExpr::Constant(DateConstant::Weekday(1))),
        "tuesday"  | "tue" => return Ok(DateExpr::Constant(DateConstant::Weekday(2))),
        "wednesday"| "wed" => return Ok(DateExpr::Constant(DateConstant::Weekday(3))),
        "thursday" | "thu" => return Ok(DateExpr::Constant(DateConstant::Weekday(4))),
        "friday"   | "fri" => return Ok(DateExpr::Constant(DateConstant::Weekday(5))),
        "saturday" | "sat" => return Ok(DateExpr::Constant(DateConstant::Weekday(6))),
        _ => {}
    }

    // last<N>days / lastNhours / lastNminutes / lastNweeks / lastNmonths
    if let Some(rest) = lower.strip_prefix("last") {
        macro_rules! try_suffix {
            ($suffix:literal, $variant:ident) => {
                if let Some(n_str) = rest.strip_suffix($suffix) {
                    if let Ok(n) = n_str.parse::<u32>() {
                        return Ok(DateExpr::Constant(DateConstant::$variant(n)));
                    }
                }
            };
        }
        try_suffix!("days",    LastNDays);
        try_suffix!("hours",   LastNHours);
        try_suffix!("minutes", LastNMinutes);
        try_suffix!("weeks",   LastNWeeks);
        try_suffix!("months",  LastNMonths);
    }

    let trimmed = s.trim();

    // Comparison operators (must use original case for date string after op)
    if let Some(rest) = trimmed.strip_prefix("<=") {
        let ts = parse_absolute_date(rest.trim())?;
        return Ok(DateExpr::Comparison(CmpOp::Lte, ts));
    }
    if let Some(rest) = trimmed.strip_prefix(">=") {
        let ts = parse_absolute_date(rest.trim())?;
        return Ok(DateExpr::Comparison(CmpOp::Gte, ts));
    }
    if let Some(rest) = trimmed.strip_prefix('<') {
        let ts = parse_absolute_date(rest.trim())?;
        return Ok(DateExpr::Comparison(CmpOp::Lt, ts));
    }
    if let Some(rest) = trimmed.strip_prefix('>') {
        let ts = parse_absolute_date(rest.trim())?;
        return Ok(DateExpr::Comparison(CmpOp::Gt, ts));
    }

    // Absolute range: date1..date2
    if let Some(pos) = trimmed.find("..") {
        let lo = parse_absolute_date(trimmed[..pos].trim())?;
        let hi = parse_absolute_date(trimmed[pos + 2..].trim())?;
        return Ok(DateExpr::Range(lo, hi));
    }

    // Single absolute date → expand to range for the whole unit (day/month/year)
    let (lo, hi) = parse_absolute_date_range(trimmed)?;
    Ok(DateExpr::Range(lo, hi))
}

/// Resolve a `DateConstant` to a `(lo, hi)` inclusive Unix-second range.
///
/// `now` carries both the wall-clock instant and the timezone offset it was
/// observed in, so "today"/"thisweek"/etc. resolve to the caller's local
/// calendar day while still producing real (offset-corrected) Unix
/// timestamps comparable against mtimes, which are always stored as UTC
/// epoch seconds. Passing a fixed `DateTime<FixedOffset>` lets callers
/// substitute a deterministic time (and offset) in tests.
pub fn resolve_date_constant(c: &DateConstant, now: DateTime<FixedOffset>) -> (i64, i64) {
    let today  = now.date_naive();
    let offset = *now.offset();
    let at = |d: NaiveDate, h: u32, m: u32, s: u32| {
        offset
            .from_local_datetime(&d.and_hms_opt(h, m, s).expect("infallible: valid h/m/s always given"))
            .single()
            .expect("infallible: FixedOffset has no DST gaps/ambiguity")
            .timestamp()
    };
    let day_s = |d: NaiveDate| at(d, 0, 0, 0);
    let day_e = |d: NaiveDate| at(d, 23, 59, 59);

    match c {
        DateConstant::Today => (day_s(today), day_e(today)),
        DateConstant::Yesterday => {
            let yd = today - Duration::days(1);
            (day_s(yd), day_e(yd))
        }
        DateConstant::ThisWeek => {
            let mon = today - Duration::days(today.weekday().num_days_from_monday() as i64);
            let sun = mon + Duration::days(6);
            (day_s(mon), day_e(sun))
        }
        DateConstant::LastWeek => {
            let this_mon = today - Duration::days(today.weekday().num_days_from_monday() as i64);
            let last_mon = this_mon - Duration::days(7);
            let last_sun = this_mon - Duration::days(1);
            (day_s(last_mon), day_e(last_sun))
        }
        DateConstant::ThisMonth => {
            let start = NaiveDate::from_ymd_opt(now.year(), now.month(), 1).expect("infallible: day 1 always valid");
            let end   = month_end(now.year(), now.month());
            (day_s(start), day_e(end))
        }
        DateConstant::LastMonth => {
            let (y, m) = prev_month(now.year(), now.month());
            let start  = NaiveDate::from_ymd_opt(y, m, 1).expect("infallible: day 1 always valid");
            let end    = month_end(y, m);
            (day_s(start), day_e(end))
        }
        DateConstant::ThisYear => {
            let start = NaiveDate::from_ymd_opt(now.year(), 1, 1).expect("infallible: Jan 1 always valid");
            let end   = NaiveDate::from_ymd_opt(now.year(), 12, 31).expect("infallible: Dec 31 always valid");
            (day_s(start), day_e(end))
        }
        DateConstant::LastYear => {
            let y     = now.year() - 1;
            let start = NaiveDate::from_ymd_opt(y, 1, 1).expect("infallible: Jan 1 always valid");
            let end   = NaiveDate::from_ymd_opt(y, 12, 31).expect("infallible: Dec 31 always valid");
            (day_s(start), day_e(end))
        }
        DateConstant::LastNDays(n) => (now.timestamp() - (*n as i64) * 86400, now.timestamp()),
        DateConstant::LastNHours(n) => (now.timestamp() - (*n as i64) * 3600, now.timestamp()),
        DateConstant::LastNMinutes(n) => (now.timestamp() - (*n as i64) * 60, now.timestamp()),
        DateConstant::LastNWeeks(n) => (now.timestamp() - (*n as i64) * 7 * 86400, now.timestamp()),
        DateConstant::LastNMonths(n) => (now.timestamp() - (*n as i64) * 30 * 86400, now.timestamp()),
        DateConstant::Month(m) => {
            let start = NaiveDate::from_ymd_opt(now.year(), *m, 1).expect("infallible: day 1 always valid");
            let end   = month_end(now.year(), *m);
            (day_s(start), day_e(end))
        }
        DateConstant::Weekday(wd) => {
            // Most recent occurrence (including today if it matches).
            // wd: 0=Sunday … 6=Saturday  (num_days_from_sunday)
            let today_wd  = today.weekday().num_days_from_sunday() as i64;
            let target_wd = *wd as i64;
            let days_ago  = (today_wd - target_wd).rem_euclid(7);
            let d         = today - Duration::days(days_ago);
            (day_s(d), day_e(d))
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn month_end(year: i32, month: u32) -> NaiveDate {
    let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    NaiveDate::from_ymd_opt(ny, nm, 1).expect("infallible: first of next month always valid") - Duration::days(1)
}

fn prev_month(year: i32, month: u32) -> (i32, u32) {
    if month == 1 { (year - 1, 12) } else { (year, month - 1) }
}

fn parse_absolute_date(s: &str) -> Result<i64> {
    // YYYY-MM-DDThh:mm:ss
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(dt.and_utc().timestamp());
    }
    // YYYY-MM-DD or YYYYMMDD
    for fmt in &["%Y-%m-%d", "%Y%m%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Ok(d.and_hms_opt(0, 0, 0).expect("infallible: 0,0,0 always valid").and_utc().timestamp());
        }
    }
    // YYYY-MM → first day of that month
    if s.len() == 7 && s.as_bytes().get(4) == Some(&b'-') {
        if let Ok(d) = NaiveDate::parse_from_str(&format!("{s}-01"), "%Y-%m-%d") {
            return Ok(d.and_hms_opt(0, 0, 0).expect("infallible: 0,0,0 always valid").and_utc().timestamp());
        }
    }
    // YYYY → first day of that year
    if s.len() == 4 {
        if let Ok(y) = s.parse::<i32>() {
            if let Some(d) = NaiveDate::from_ymd_opt(y, 1, 1) {
                return Ok(d.and_hms_opt(0, 0, 0).expect("infallible: 0,0,0 always valid").and_utc().timestamp());
            }
        }
    }
    Err(AppError::Query(format!("invalid date: '{s}'")))
}

/// Like `parse_absolute_date` but expands to a (lo, hi) range covering
/// the whole unit implied by the format (day → 24 h, month → whole month, year → whole year).
fn parse_absolute_date_range(s: &str) -> Result<(i64, i64)> {
    let lo = parse_absolute_date(s)?;

    // Determine granularity from string shape
    if s.contains('T') {
        // Exact second
        return Ok((lo, lo));
    }
    let len = s.len();
    if len == 10 || len == 8 {
        // YYYY-MM-DD or YYYYMMDD - whole day
        return Ok((lo, lo + 86400 - 1));
    }
    if len == 7 {
        // YYYY-MM - whole month
        let parts: Vec<&str> = s.splitn(2, '-').collect();
        if parts.len() == 2 {
            if let (Ok(y), Ok(m)) = (parts[0].parse::<i32>(), parts[1].parse::<u32>()) {
                let end_ts = NaiveDate::from_ymd_opt(y, m, 1)
                    .map(|_d| {
                        let last = month_end(y, m);
                        last.and_hms_opt(23, 59, 59).expect("infallible: 23,59,59 always valid").and_utc().timestamp()
                    })
                    .unwrap_or(lo + 86400 - 1);
                return Ok((lo, end_ts));
            }
        }
    }
    if len == 4 {
        // YYYY - whole year
        if let Ok(y) = s.parse::<i32>() {
            let hi = NaiveDate::from_ymd_opt(y, 12, 31)
                .map(|d| d.and_hms_opt(23, 59, 59).expect("infallible: 23,59,59 always valid").and_utc().timestamp())
                .unwrap_or(lo + 365 * 86400);
            return Ok((lo, hi));
        }
    }
    Ok((lo, lo + 86400 - 1))
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn fixed_now_at(offset_hours: i32) -> DateTime<FixedOffset> {
        // 2024-03-15 12:00:00 (Friday), observed at the given UTC offset.
        let offset = FixedOffset::east_opt(offset_hours * 3600).unwrap();
        offset
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(2024, 3, 15)
                    .unwrap()
                    .and_hms_opt(12, 0, 0)
                    .unwrap(),
            )
            .unwrap()
    }

    fn fixed_now() -> DateTime<FixedOffset> {
        fixed_now_at(0)
    }

    #[test]
    fn today_range() {
        let now = fixed_now();
        let (lo, hi) = resolve_date_constant(&DateConstant::Today, now);
        let start = NaiveDate::from_ymd_opt(2024, 3, 15).unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
        let end   = NaiveDate::from_ymd_opt(2024, 3, 15).unwrap().and_hms_opt(23, 59, 59).unwrap().and_utc().timestamp();
        assert_eq!(lo, start);
        assert_eq!(hi, end);
    }

    #[test]
    fn today_range_respects_offset() {
        // At UTC+7, local midnight of 2024-03-15 is 2024-03-14 17:00:00 UTC,
        // not 2024-03-15 00:00:00 UTC. A file whose mtime is a few hours
        // before local midnight-in-UTC-digits must still fall inside "today".
        let now = fixed_now_at(7);
        let (lo, hi) = resolve_date_constant(&DateConstant::Today, now);
        let expected_lo = NaiveDate::from_ymd_opt(2024, 3, 14).unwrap().and_hms_opt(17, 0, 0).unwrap().and_utc().timestamp();
        let expected_hi = NaiveDate::from_ymd_opt(2024, 3, 15).unwrap().and_hms_opt(16, 59, 59).unwrap().and_utc().timestamp();
        assert_eq!(lo, expected_lo);
        assert_eq!(hi, expected_hi);
    }

    #[test]
    fn thisweek_monday_to_sunday() {
        let now = fixed_now(); // Friday 2024-03-15
        let (lo, _hi) = resolve_date_constant(&DateConstant::ThisWeek, now);
        // Monday 2024-03-11
        let mon = NaiveDate::from_ymd_opt(2024, 3, 11).unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
        assert_eq!(lo, mon);
    }

    #[test]
    fn last7days_range() {
        let now = fixed_now();
        let now_ts = now.timestamp();
        let (lo, hi) = resolve_date_constant(&DateConstant::LastNDays(7), now);
        assert_eq!(hi, now_ts);
        assert_eq!(lo, now_ts - 7 * 86400);
    }

    #[test]
    fn parse_today_constant() {
        let e = parse_date_value("today").unwrap();
        assert!(matches!(e, DateExpr::Constant(DateConstant::Today)));
    }

    #[test]
    fn parse_absolute_day() {
        let e = parse_date_value("2024-01-01").unwrap();
        if let DateExpr::Range(lo, hi) = e {
            assert!(hi > lo);
            assert_eq!(hi - lo, 86399);
        } else {
            panic!("expected Range");
        }
    }

    #[test]
    fn parse_last14days() {
        let e = parse_date_value("last14days").unwrap();
        assert!(matches!(e, DateExpr::Constant(DateConstant::LastNDays(14))));
    }
}
