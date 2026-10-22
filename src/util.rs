//! Calendar dates without a date library: Unix days ↔ (year, month, day).

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Howard Hinnant's civil_from_days.
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// `22-Oct-2026 14:03:00 +0000` (IMAP INTERNALDATE).
pub fn imap_datetime(unix: i64) -> String {
    let (y, m, d) = civil(unix.div_euclid(86400));
    let s = unix.rem_euclid(86400);
    format!("{d:02}-{}-{y:04} {:02}:{:02}:{:02} +0000", MONTHS[m as usize - 1], s / 3600, s % 3600 / 60, s % 60)
}

/// `22-Oct-2026` (IMAP SEARCH dates).
pub fn imap_date(unix: i64) -> String {
    let (y, m, d) = civil(unix.div_euclid(86400));
    format!("{d}-{}-{y:04}", MONTHS[m as usize - 1])
}

/// Unix days of an IMAP date (`1-Feb-1994`).
pub fn parse_imap_date(s: &str) -> Option<i64> {
    let mut it = s.trim_matches('"').split('-');
    let d: u32 = it.next()?.trim().parse().ok()?;
    let mon = it.next()?;
    let m = MONTHS.iter().position(|x| x.eq_ignore_ascii_case(mon))? as u32 + 1;
    let y: i64 = it.next()?.get(..4)?.parse().ok()?;
    Some(days_from_civil(y, m, d))
}

/// Unix seconds of an IMAP date-time (`22-Oct-2026 14:03:00 +0200`).
pub fn parse_imap_datetime(s: &str) -> Option<i64> {
    let s = s.trim_matches('"').trim();
    let (date, rest) = s.split_once(' ')?;
    let days = parse_imap_date(date)?;
    let (time, zone) = rest.trim().split_once(' ').unwrap_or((rest.trim(), "+0000"));
    let mut t = time.split(':').map(|p| p.parse::<i64>().ok());
    let (h, mi, se) = (t.next()??, t.next()??, t.next().flatten().unwrap_or(0));
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    let z: i64 = zone.trim_start_matches(['+', '-']).parse().ok()?;
    Some(days * 86400 + h * 3600 + mi * 60 + se - sign * (z / 100 * 3600 + z % 100 * 60))
}

/// `Thu, 22 Oct 2026 14:03:00 +0000` (email Date header).
pub fn rfc2822(unix: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    let days = unix.div_euclid(86400);
    let (y, m, d) = civil(days);
    let s = unix.rem_euclid(86400);
    format!(
        "{}, {d} {} {y:04} {:02}:{:02}:{:02} +0000",
        DAYS[days.rem_euclid(7) as usize],
        MONTHS[m as usize - 1],
        s / 3600,
        s % 3600 / 60,
        s % 60
    )
}

/// Today as `YYYYMMDD` (UTC).
pub fn today() -> String {
    let (y, m, d) = civil(crate::mailstore::now().div_euclid(86400));
    format!("{y:04}{m:02}{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2026, 10, 22), 20748);
        assert_eq!(civil(20748), (2026, 10, 22));
        assert_eq!(imap_datetime(20748 * 86400 + 3661), "22-Oct-2026 01:01:01 +0000");
        assert_eq!(parse_imap_date("1-Feb-1994"), Some(days_from_civil(1994, 2, 1)));
        assert_eq!(parse_imap_datetime("22-Oct-2026 02:01:01 +0100"), Some(20748 * 86400 + 3661));
        assert_eq!(imap_date(20748 * 86400), "22-Oct-2026");
        assert_eq!(rfc2822(20748 * 86400), "Thu, 22 Oct 2026 00:00:00 +0000");
    }
}
