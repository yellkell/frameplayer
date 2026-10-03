//! Date parsing for HTTP headers, WebDAV properties and directory listings,
//! without pulling in a calendar crate. Every result is Unix seconds (UTC).

use std::time::{SystemTime, UNIX_EPOCH};

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn unix(year: i64, month: u32, day: u32, h: u32, mi: u32, s: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + i64::from(h * 3600 + mi * 60 + s))
}

fn month_from_name(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lower = name.get(..3)?.to_ascii_lowercase();
    MONTHS
        .iter()
        .position(|m| *m == lower)
        .map(|i| i as u32 + 1)
}

/// Parses `HH:MM` or `HH:MM:SS` (fractions after the seconds are ignored).
fn parse_clock(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.split(':');
    let h = parts.next()?.parse().ok()?;
    let m = parts.next()?.parse().ok()?;
    let sec = match parts.next() {
        Some(x) => x.split('.').next()?.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((h, m, sec))
}

fn full_year(y: i64) -> i64 {
    match y {
        0..=69 => 2000 + y,
        70..=99 => 1900 + y,
        _ => y,
    }
}

/// Parses the three HTTP date formats (RFC 1123, RFC 850, asctime), as used
/// by `Last-Modified` and WebDAV `getlastmodified`. Assumes GMT.
pub fn parse_http_date(s: &str) -> Option<i64> {
    let s = s.trim();
    // Drop the weekday ("Sun," / "Sunday,").
    let rest = match s.split_once(',') {
        Some((_, r)) => r.trim(),
        None => s.split_once(' ').map(|(_, r)| r.trim())?,
    };
    let toks: Vec<&str> = rest.split_whitespace().collect();
    if s.contains(',') {
        // RFC 1123: "06 Nov 1994 08:49:37 GMT"; RFC 850: "06-Nov-94 08:49:37 GMT".
        let (day, mon, year, clock) = if toks.first()?.contains('-') {
            let mut d = toks.first()?.split('-');
            (d.next()?, d.next()?, d.next()?, *toks.get(1)?)
        } else {
            (*toks.first()?, *toks.get(1)?, *toks.get(2)?, *toks.get(3)?)
        };
        let (h, mi, sec) = parse_clock(clock)?;
        unix(
            full_year(year.parse().ok()?),
            month_from_name(mon)?,
            day.parse().ok()?,
            h,
            mi,
            sec,
        )
    } else {
        // asctime: "Nov  6 08:49:37 1994".
        let (h, mi, sec) = parse_clock(toks.get(2)?)?;
        unix(
            toks.get(3)?.parse().ok()?,
            month_from_name(toks.first()?)?,
            toks.get(1)?.parse().ok()?,
            h,
            mi,
            sec,
        )
    }
}

/// Parses an ISO 8601 / RFC 3339 timestamp (`2024-01-12T10:22:33Z`,
/// `2024-01-12T10:22:33.5+01:00`, or a plain `2024-01-12`).
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = match s.find(['T', 't', ' ']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: u32 = d.next()?.parse().ok()?;
    let da: u32 = d.next()?.parse().ok()?;
    let Some(time) = time else {
        return unix(y, mo, da, 0, 0, 0);
    };
    let (clock, offset) = match time.find(['Z', 'z', '+', '-']) {
        Some(i) => (&time[..i], &time[i..]),
        None => (time, ""),
    };
    let (h, mi, sec) = parse_clock(clock)?;
    let base = unix(y, mo, da, h, mi, sec)?;
    let off = match offset.chars().next() {
        Some(sign @ ('+' | '-')) => {
            let digits: String = offset[1..].chars().filter(char::is_ascii_digit).collect();
            let oh: i64 = digits.get(..2)?.parse().ok()?;
            let om: i64 = digits.get(2..4).and_then(|m| m.parse().ok()).unwrap_or(0);
            let secs = oh * 3600 + om * 60;
            if sign == '+' { secs } else { -secs }
        }
        _ => 0,
    };
    Some(base - off)
}

/// Parses the dates that Apache, nginx and lighttpd print in autoindex
/// pages: `12-Jan-2024 10:22`, `2024-01-12 10:22`, `2024-Jan-12 10:22:33`.
/// `date` and `clock` are the two whitespace-separated tokens.
pub fn parse_listing_date(date: &str, clock: &str) -> Option<i64> {
    let (h, mi, sec) = parse_clock(clock)?;
    let parts: Vec<&str> = date.split(['-', '/']).collect();
    if parts.len() != 3 {
        return None;
    }
    let (y, mo, d) = if parts[0].len() == 4 {
        // 2024-01-12 or 2024-Jan-12
        let mo = parts[1]
            .parse()
            .ok()
            .or_else(|| month_from_name(parts[1]))?;
        (parts[0].parse().ok()?, mo, parts[2].parse().ok()?)
    } else {
        // 12-Jan-2024
        (
            full_year(parts[2].parse().ok()?),
            month_from_name(parts[1])?,
            parts[0].parse().ok()?,
        )
    };
    unix(y, mo, d, h, mi, sec)
}

/// Converts a Windows FILETIME (100 ns ticks since 1601) to Unix seconds.
pub fn filetime_to_unix(ticks: u64) -> Option<i64> {
    if ticks == 0 {
        return None;
    }
    Some((ticks / 10_000_000) as i64 - 11_644_473_600)
}

/// Converts a `SystemTime` to Unix seconds (negative before 1970).
pub fn system_time_to_unix(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// Parses a duration: plain seconds (`"95.5"`) or clock form
/// `H:MM:SS(.fff)` / `MM:SS` as DLNA `res@duration` uses it. DLNA's
/// fractional form `S.F0/F1` is accepted too.
pub fn parse_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total = 0.0;
    for part in s.split(':') {
        let v = match part.split_once('/') {
            Some((num, den)) => {
                // "S.F0/F1": whole seconds plus fraction F0/F1.
                let (whole, f0) = num.split_once('.').unwrap_or((num, "0"));
                let f0: f64 = f0.parse().ok()?;
                let f1: f64 = den.parse().ok()?;
                whole.parse::<f64>().ok()? + if f1 > 0.0 { f0 / f1 } else { 0.0 }
            }
            None => part.parse::<f64>().ok()?,
        };
        if !v.is_finite() || v < 0.0 {
            return None;
        }
        total = total * 60.0 + v;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
    }

    #[test]
    fn http_dates() {
        let want = Some(784_111_777);
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), want);
        assert_eq!(parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"), want);
        assert_eq!(parse_http_date("Sun Nov  6 08:49:37 1994"), want);
        assert_eq!(
            parse_http_date("Fri, 12 Jan 2024 10:22:33 GMT"),
            Some(1_705_054_953)
        );
        assert_eq!(parse_http_date("garbage"), None);
        assert_eq!(parse_http_date(""), None);
    }

    #[test]
    fn iso_dates() {
        assert_eq!(parse_iso8601("2024-01-12T10:22:33Z"), Some(1_705_054_953));
        assert_eq!(
            parse_iso8601("2024-01-12T11:22:33.250+01:00"),
            Some(1_705_054_953)
        );
        assert_eq!(parse_iso8601("2024-01-12"), Some(1_705_017_600));
        assert_eq!(parse_iso8601("yesterday"), None);
    }

    #[test]
    fn listing_dates() {
        let want = Some(1_705_054_920);
        assert_eq!(parse_listing_date("12-Jan-2024", "10:22"), want);
        assert_eq!(parse_listing_date("2024-01-12", "10:22"), want);
        assert_eq!(parse_listing_date("2024-Jan-12", "10:22:00"), want);
        assert_eq!(parse_listing_date("12-Foo-2024", "10:22"), None);
        assert_eq!(parse_listing_date("2024-01-12", "25:00"), None);
    }

    #[test]
    fn filetime() {
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), Some(0));
        assert_eq!(filetime_to_unix(0), None);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("1:02:03.500"), Some(3723.5));
        assert_eq!(parse_duration("0:00:05"), Some(5.0));
        assert_eq!(parse_duration("12:34"), Some(754.0));
        assert_eq!(parse_duration("95.25"), Some(95.25));
        assert_eq!(parse_duration("0:00:01.1/2"), Some(1.5));
        assert_eq!(parse_duration("x:1"), None);
        assert_eq!(parse_duration(""), None);
    }
}
