use chrono::{
    DateTime, Datelike, Duration, FixedOffset, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone,
};

use super::ast::{DateConstant, DateExpr};
use crate::error::{AppError, Result};
use crate::store::CmpOp;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn parse_date_value(s: &str) -> Result<DateExpr> {
    let s = s.trim();
    for (op_str, op) in [
        ("<=", CmpOp::Lte),
        (">=", CmpOp::Gte),
        ("<", CmpOp::Lt),
        (">", CmpOp::Gt),
        ("=", CmpOp::Eq),
    ] {
        if let Some(rest) = s.strip_prefix(op_str) {
            return Ok(DateExpr::Comparison(
                op,
                Box::new(parse_period(rest.trim())?),
            ));
        }
    }
    if let Some((a, b)) = s.split_once("..") {
        return Ok(DateExpr::Between(
            Box::new(parse_period(a.trim())?),
            Box::new(parse_period(b.trim())?),
        ));
    }
    if let Ok(e) = parse_period(s) {
        return Ok(e);
    }
    // `a-b` ranges: dates contain dashes too, so take the first split where
    // both halves are valid (`2024-01-01-2024-12-31`, `2023-2024`).
    for (i, _) in s.match_indices('-') {
        if let (Ok(a), Ok(b)) = (parse_period(&s[..i]), parse_period(&s[i + 1..])) {
            return Ok(DateExpr::Between(Box::new(a), Box::new(b)));
        }
    }
    Err(AppError::Query(format!("invalid date: '{s}'")))
}

type MakeConstant = fn(u32) -> DateConstant;

/// One named or absolute period: `today`, `jan`, `last3days`, `2024`,
/// `2024-01`, `2024-01-15`, `20240115`, `2024-01-15T10:00:00`.
fn parse_period(s: &str) -> Result<DateExpr> {
    let lower = s.to_ascii_lowercase();
    let constant = match lower.as_str() {
        "today" => Some(DateConstant::Today),
        "yesterday" => Some(DateConstant::Yesterday),
        "thisweek" => Some(DateConstant::ThisWeek),
        "lastweek" => Some(DateConstant::LastWeek),
        "thismonth" => Some(DateConstant::ThisMonth),
        "lastmonth" => Some(DateConstant::LastMonth),
        "thisyear" => Some(DateConstant::ThisYear),
        "lastyear" => Some(DateConstant::LastYear),
        "january" | "jan" => Some(DateConstant::Month(1)),
        "february" | "feb" => Some(DateConstant::Month(2)),
        "march" | "mar" => Some(DateConstant::Month(3)),
        "april" | "apr" => Some(DateConstant::Month(4)),
        "may" => Some(DateConstant::Month(5)),
        "june" | "jun" => Some(DateConstant::Month(6)),
        "july" | "jul" => Some(DateConstant::Month(7)),
        "august" | "aug" => Some(DateConstant::Month(8)),
        "september" | "sep" => Some(DateConstant::Month(9)),
        "october" | "oct" => Some(DateConstant::Month(10)),
        "november" | "nov" => Some(DateConstant::Month(11)),
        "december" | "dec" => Some(DateConstant::Month(12)),
        "sunday" | "sun" => Some(DateConstant::Weekday(0)),
        "monday" | "mon" => Some(DateConstant::Weekday(1)),
        "tuesday" | "tue" => Some(DateConstant::Weekday(2)),
        "wednesday" | "wed" => Some(DateConstant::Weekday(3)),
        "thursday" | "thu" => Some(DateConstant::Weekday(4)),
        "friday" | "fri" => Some(DateConstant::Weekday(5)),
        "saturday" | "sat" => Some(DateConstant::Weekday(6)),
        _ => None,
    };
    if let Some(c) = constant {
        return Ok(DateExpr::Constant(c));
    }

    if let Some(rest) = lower.strip_prefix("last") {
        let units: [(&str, MakeConstant); 5] = [
            ("days", DateConstant::LastNDays),
            ("hours", DateConstant::LastNHours),
            ("minutes", DateConstant::LastNMinutes),
            ("weeks", DateConstant::LastNWeeks),
            ("months", DateConstant::LastNMonths),
        ];
        for (suffix, make) in units {
            if let Some(n) = rest.strip_suffix(suffix).and_then(|n| n.parse().ok()) {
                return Ok(DateExpr::Constant(make(n)));
            }
        }
    }

    let (lo, hi) = parse_absolute_date_range(s)?;
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
    let today = now.date_naive();
    let offset = *now.offset();
    let at = |d: NaiveDate, h: u32, m: u32, s: u32| {
        offset
            .from_local_datetime(
                &d.and_hms_opt(h, m, s)
                    .expect("infallible: valid h/m/s always given"),
            )
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
            let start = NaiveDate::from_ymd_opt(now.year(), now.month(), 1)
                .expect("infallible: day 1 always valid");
            let end = month_end(now.year(), now.month());
            (day_s(start), day_e(end))
        }
        DateConstant::LastMonth => {
            let (y, m) = prev_month(now.year(), now.month());
            let start = NaiveDate::from_ymd_opt(y, m, 1).expect("infallible: day 1 always valid");
            let end = month_end(y, m);
            (day_s(start), day_e(end))
        }
        DateConstant::ThisYear => {
            let start =
                NaiveDate::from_ymd_opt(now.year(), 1, 1).expect("infallible: Jan 1 always valid");
            let end = NaiveDate::from_ymd_opt(now.year(), 12, 31)
                .expect("infallible: Dec 31 always valid");
            (day_s(start), day_e(end))
        }
        DateConstant::LastYear => {
            let y = now.year() - 1;
            let start = NaiveDate::from_ymd_opt(y, 1, 1).expect("infallible: Jan 1 always valid");
            let end = NaiveDate::from_ymd_opt(y, 12, 31).expect("infallible: Dec 31 always valid");
            (day_s(start), day_e(end))
        }
        DateConstant::LastNDays(n) => (now.timestamp() - (*n as i64) * 86400, now.timestamp()),
        DateConstant::LastNHours(n) => (now.timestamp() - (*n as i64) * 3600, now.timestamp()),
        DateConstant::LastNMinutes(n) => (now.timestamp() - (*n as i64) * 60, now.timestamp()),
        DateConstant::LastNWeeks(n) => (now.timestamp() - (*n as i64) * 7 * 86400, now.timestamp()),
        DateConstant::LastNMonths(n) => {
            (now.timestamp() - (*n as i64) * 30 * 86400, now.timestamp())
        }
        DateConstant::Month(m) => {
            let start =
                NaiveDate::from_ymd_opt(now.year(), *m, 1).expect("infallible: day 1 always valid");
            let end = month_end(now.year(), *m);
            (day_s(start), day_e(end))
        }
        DateConstant::Weekday(wd) => {
            // Most recent occurrence (including today if it matches).
            // wd: 0=Sunday … 6=Saturday  (num_days_from_sunday)
            let today_wd = today.weekday().num_days_from_sunday() as i64;
            let target_wd = *wd as i64;
            let days_ago = (today_wd - target_wd).rem_euclid(7);
            let d = today - Duration::days(days_ago);
            (day_s(d), day_e(d))
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn month_end(year: i32, month: u32) -> NaiveDate {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(ny, nm, 1).expect("infallible: first of next month always valid")
        - Duration::days(1)
}

fn prev_month(year: i32, month: u32) -> (i32, u32) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

/// Typed dates are local wall-clock time, like `today` and the other
/// constants. A time skipped by a DST jump falls back to its UTC reading.
fn local_ts(dt: NaiveDateTime) -> i64 {
    Local
        .from_local_datetime(&dt)
        .earliest()
        .map_or_else(|| dt.and_utc().timestamp(), |t| t.timestamp())
}

fn day_start(d: NaiveDate) -> i64 {
    local_ts(d.and_time(NaiveTime::MIN))
}

/// Last second of the period `[from, next)`, found from the next period's
/// start so DST days and leap years need no special casing.
fn until(next: NaiveDate) -> i64 {
    day_start(next) - 1
}

/// The whole period implied by the format: an exact second, a day, a month
/// or a year, as an inclusive `(lo, hi)` in Unix seconds.
fn parse_absolute_date_range(s: &str) -> Result<(i64, i64)> {
    let invalid = || AppError::Query(format!("invalid date: '{s}'"));
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        let t = local_ts(dt);
        return Ok((t, t));
    }
    for fmt in ["%Y-%m-%d", "%Y%m%d"] {
        if s.len() == if fmt == "%Y%m%d" { 8 } else { 10 } {
            if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
                let next = d.succ_opt().ok_or_else(invalid)?;
                return Ok((day_start(d), until(next)));
            }
        }
    }
    if s.len() == 7 && s.as_bytes()[4] == b'-' {
        if let Ok(d) = NaiveDate::parse_from_str(&format!("{s}-01"), "%Y-%m-%d") {
            let (ny, nm) = if d.month() == 12 {
                (d.year() + 1, 1)
            } else {
                (d.year(), d.month() + 1)
            };
            let next = NaiveDate::from_ymd_opt(ny, nm, 1).ok_or_else(invalid)?;
            return Ok((day_start(d), until(next)));
        }
    }
    if s.len() == 4 && s.bytes().all(|b| b.is_ascii_digit()) {
        let y: i32 = s.parse().map_err(|_| invalid())?;
        let first = NaiveDate::from_ymd_opt(y, 1, 1).ok_or_else(invalid)?;
        let next = NaiveDate::from_ymd_opt(y + 1, 1, 1).ok_or_else(invalid)?;
        return Ok((day_start(first), until(next)));
    }
    Err(invalid())
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
        let start = NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        let end = NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap()
            .and_utc()
            .timestamp();
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
        let expected_lo = NaiveDate::from_ymd_opt(2024, 3, 14)
            .unwrap()
            .and_hms_opt(17, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        let expected_hi = NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(16, 59, 59)
            .unwrap()
            .and_utc()
            .timestamp();
        assert_eq!(lo, expected_lo);
        assert_eq!(hi, expected_hi);
    }

    #[test]
    fn thisweek_monday_to_sunday() {
        let now = fixed_now(); // Friday 2024-03-15
        let (lo, _hi) = resolve_date_constant(&DateConstant::ThisWeek, now);
        // Monday 2024-03-11
        let mon = NaiveDate::from_ymd_opt(2024, 3, 11)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
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
