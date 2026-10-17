//! Date/time functions.

use super::expr::eval_expr;
use super::value::{Row, Value};
use sqlparser::ast::{DateTimeField, Expr};

/// Returns the current UTC time formatted for `now()`/`current_timestamp`/
/// `current_date`/`current_time`. No date columns exist in the schema, so
/// this only needs to support clock-style scalar lookups, not arithmetic.
pub(super) fn current_datetime_utc(with_date: bool, with_time: bool) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86400) as i64;
    let time_of_day = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (
        time_of_day / 3600,
        (time_of_day / 60) % 60,
        time_of_day % 60,
    );
    match (with_date, with_time) {
        (true, true) => format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}"),
        (true, false) => format!("{y:04}-{m:02}-{d:02}"),
        _ => format!("{h:02}:{mi:02}:{s:02}"),
    }
}

/// Howard Hinnant's `civil_from_days` algorithm: converts a day count
/// since the Unix epoch (1970-01-01) into a proleptic-Gregorian (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(y) => 29,
        2 => 28,
        _ => 30,
    }
}

fn parse_datetime(s: &str) -> Option<(i64, i64)> {
    let s = s.trim();
    let (date_part, time_part) = match s.find(['T', ' ']) {
        Some(idx) => (&s[..idx], Some(&s[idx + 1..])),
        None => (s, None),
    };
    let mut parts = date_part.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, m, d);
    let secs = match time_part {
        Some(tp) => {
            let tp = tp.trim_end_matches('Z');
            let tp = tp.split(['+']).next().unwrap_or(tp);
            let tp = match tp.rfind('-') {
                Some(pos) => &tp[..pos],
                None => tp,
            };
            let tp = tp.split('.').next().unwrap_or(tp);
            let mut tparts = tp.splitn(3, ':');
            let h: i64 = tparts.next()?.parse().ok()?;
            let mi: i64 = tparts.next().unwrap_or("0").parse().ok()?;
            let se: i64 = tparts.next().unwrap_or("0").parse().ok()?;
            h * 3600 + mi * 60 + se
        }
        None => 0,
    };
    Some((days, secs))
}

fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn format_datetime(days: i64, secs: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

fn weekday_from_days(days: i64) -> u32 {
    (((days % 7) + 11) % 7) as u32
}

const WEEKDAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

pub(super) fn eval_extract(field: &DateTimeField, expr: &Expr, row: &Row) -> Value {
    let Some(s) = eval_expr(expr, row).as_str().map(str::to_string) else {
        return Value::Null;
    };
    let Some((days, secs)) = parse_datetime(&s) else {
        return Value::Null;
    };
    let (y, m, d) = civil_from_days(days);
    match field {
        DateTimeField::Year | DateTimeField::Years => Value::Int(y),
        DateTimeField::Month | DateTimeField::Months => Value::Int(m as i64),
        DateTimeField::Day | DateTimeField::Days => Value::Int(d as i64),
        DateTimeField::Hour | DateTimeField::Hours => Value::Int(secs / 3600),
        DateTimeField::Minute | DateTimeField::Minutes => Value::Int((secs / 60) % 60),
        DateTimeField::Second | DateTimeField::Seconds => Value::Int(secs % 60),
        DateTimeField::Dow => Value::Int(weekday_from_days(days) as i64),
        DateTimeField::Doy => Value::Int(days - days_from_civil(y, 1, 1) + 1),
        DateTimeField::Epoch => Value::Int(days * 86400 + secs),
        _ => Value::Null,
    }
}

fn add_months(days: i64, n: i64) -> i64 {
    let (y, m, d) = civil_from_days(days);
    let total_months = y * 12 + (m as i64 - 1) + n;
    let ny = total_months.div_euclid(12);
    let nm = (total_months.rem_euclid(12) + 1) as u32;
    let nd = d.min(days_in_month(ny, nm));
    days_from_civil(ny, nm, nd)
}

pub(super) fn eval_date_trunc(unit: &str, date: &str) -> Value {
    let Some((days, secs)) = parse_datetime(date) else {
        return Value::Null;
    };
    let (y, m, _) = civil_from_days(days);
    match unit.to_lowercase().as_str() {
        "year" => Value::Str(format_date(days_from_civil(y, 1, 1))),
        "month" => Value::Str(format_date(days_from_civil(y, m, 1))),
        "day" => Value::Str(format_date(days)),
        "hour" => Value::Str(format_datetime(days, (secs / 3600) * 3600)),
        "minute" => Value::Str(format_datetime(days, (secs / 60) * 60)),
        _ => Value::Null,
    }
}

pub(super) fn eval_date_diff(unit: &str, date1: &str, date2: &str) -> Value {
    let (Some((days1, secs1)), Some((days2, secs2))) =
        (parse_datetime(date1), parse_datetime(date2))
    else {
        return Value::Null;
    };
    match unit.to_lowercase().as_str() {
        "day" => Value::Int(days2 - days1),
        "second" => Value::Int((days2 - days1) * 86400 + (secs2 - secs1)),
        "month" => {
            let (y1, m1, _) = civil_from_days(days1);
            let (y2, m2, _) = civil_from_days(days2);
            Value::Int((y2 * 12 + m2 as i64) - (y1 * 12 + m1 as i64))
        }
        "year" => {
            let (y1, _, _) = civil_from_days(days1);
            let (y2, _, _) = civil_from_days(days2);
            Value::Int(y2 - y1)
        }
        _ => Value::Null,
    }
}

pub(super) fn eval_date_add(date: &str, n: i64, unit: &str) -> Value {
    let Some((days, secs)) = parse_datetime(date) else {
        return Value::Null;
    };
    let with_time = secs != 0 || date.contains([' ', 'T']);
    let render = |days: i64, secs: i64| {
        if with_time {
            format_datetime(days, secs)
        } else {
            format_date(days)
        }
    };
    match unit.to_lowercase().as_str() {
        "day" => Value::Str(render(days + n, secs)),
        "month" => Value::Str(render(add_months(days, n), secs)),
        "year" => Value::Str(render(add_months(days, n * 12), secs)),
        "hour" => Value::Str(render(days, secs + n * 3600)),
        "minute" => Value::Str(render(days, secs + n * 60)),
        "second" => Value::Str(render(days, secs + n)),
        _ => Value::Null,
    }
}

pub(super) fn eval_strftime(format: &str, date: &str) -> Value {
    let Some((days, secs)) = parse_datetime(date) else {
        return Value::Null;
    };
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    let wd = weekday_from_days(days) as usize;
    let doy = days - days_from_civil(y, 1, 1) + 1;
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{y:04}")),
            Some('m') => out.push_str(&format!("{m:02}")),
            Some('d') => out.push_str(&format!("{d:02}")),
            Some('H') => out.push_str(&format!("{h:02}")),
            Some('M') => out.push_str(&format!("{mi:02}")),
            Some('S') => out.push_str(&format!("{s:02}")),
            Some('j') => out.push_str(&format!("{doy:03}")),
            Some('w') => out.push_str(&wd.to_string()),
            Some('A') => out.push_str(WEEKDAY_NAMES[wd]),
            Some('a') => out.push_str(&WEEKDAY_NAMES[wd][..3]),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    Value::Str(out)
}
