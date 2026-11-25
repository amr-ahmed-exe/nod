use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, SecondsFormat, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn sha256_hex(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn base64url_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let combined = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((combined >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((combined >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((combined >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[(combined & 0x3f) as usize] as char);
        }
    }
    out
}

pub fn random_base64url(num_bytes: usize) -> String {
    let mut buf = vec![0u8; num_bytes];
    if getrandom::fill(&mut buf).is_err() {
        let fallback = format!("{}{}", uuid(), uuid());
        return base64url_encode(fallback.as_bytes());
    }
    base64url_encode(&buf)
}

pub fn url_encode_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for &byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0').to_ascii_uppercase());
                out.push(char::from_digit(u32::from(byte & 0xf), 16).unwrap_or('0').to_ascii_uppercase());
            }
        }
    }
    out
}

pub fn uuid() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let high = nanos ^ sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let low = nanos.rotate_left(17)
        ^ sequence.wrapping_mul(0xbf58_476d_1ce4_e5b9)
        ^ u64::from(std::process::id());
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        high >> 32,
        (high >> 16) & 0xffff,
        high & 0x0fff,
        0x8000 | ((low >> 48) & 0x3fff),
        low & 0xffff_ffff_ffff
    )
}

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

pub fn iso(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn iso_now() -> String {
    iso(now())
}

pub fn parse_iso(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

pub fn zone(name: &str) -> Tz {
    name.parse::<Tz>().unwrap_or(chrono_tz::UTC)
}

pub fn local_zone() -> Tz {
    iana_time_zone()
        .and_then(|name| name.parse::<Tz>().ok())
        .unwrap_or(chrono_tz::UTC)
}

fn iana_time_zone() -> Option<String> {
    if let Ok(value) = std::env::var("TZ")
        && value.parse::<Tz>().is_ok() {
            return Some(value);
        }
    iana_time_zone::get_timezone()
        .ok()
        .filter(|name| name.parse::<Tz>().is_ok())
}

#[derive(Clone, Debug)]
pub struct Cron {
    minutes: Vec<u32>,
    hours: Vec<u32>,
    days: Vec<u32>,
    months: Vec<u32>,
    weekdays: Vec<u32>,
    days_any: bool,
    weekdays_any: bool,
}

impl Cron {
    pub fn parse(expression: &str) -> Option<Self> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        if fields.len() != 5 {
            return None;
        }
        let minutes = parse_field(fields[0], 0, 59)?;
        let hours = parse_field(fields[1], 0, 23)?;
        let days = parse_field(fields[2], 1, 31)?;
        let months = parse_field(fields[3], 1, 12)?;
        let weekdays = parse_field(fields[4], 0, 7)?
            .into_iter()
            .map(|value| value % 7)
            .collect();
        Some(Self {
            days_any: fields[2].trim() == "*",
            weekdays_any: fields[4].trim() == "*",
            minutes,
            hours,
            days,
            months,
            weekdays,
        })
    }

    fn matches_day(&self, date: NaiveDate) -> bool {
        if !self.months.contains(&date.month()) {
            return false;
        }
        let day = self.days.contains(&date.day());
        let weekday = self.weekdays.contains(&date.weekday().num_days_from_sunday());
        if self.days_any && self.weekdays_any {
            return true;
        }
        if self.days_any {
            return weekday;
        }
        if self.weekdays_any {
            return day;
        }
        day || weekday
    }

    pub fn next_after(&self, after: DateTime<Tz>) -> Option<DateTime<Tz>> {
        let mut date = after.date_naive();
        let start = after;
        for _ in 0..(366 * 6) {
            if self.matches_day(date) {
                for hour in &self.hours {
                    for minute in &self.minutes {
                        let naive = date.and_hms_opt(*hour, *minute, 0)?;
                        let candidate = match zone_local(&start.timezone(), naive) {
                            LocalResult::Single(value) => value,
                            LocalResult::Ambiguous(first, _) => first,
                            LocalResult::None => continue,
                        };
                        if candidate > start {
                            return Some(candidate);
                        }
                    }
                }
            }
            date = date.succ_opt()?;
        }
        None
    }

    pub fn next_runs(&self, from: DateTime<Utc>, tz: Tz, count: usize) -> Vec<DateTime<Utc>> {
        let mut results = Vec::with_capacity(count);
        let mut cursor = from.with_timezone(&tz);
        for _ in 0..count {
            let Some(next) = self.next_after(cursor) else {
                break;
            };
            results.push(next.with_timezone(&Utc));
            cursor = next;
        }
        results
    }
}

fn zone_local(tz: &Tz, naive: chrono::NaiveDateTime) -> LocalResult<DateTime<Tz>> {
    tz.from_local_datetime(&naive)
}

fn parse_field(field: &str, minimum: u32, maximum: u32) -> Option<Vec<u32>> {
    let mut values = Vec::new();
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, step.parse::<u32>().ok()?),
            None => (part, 1),
        };
        if step == 0 {
            return None;
        }
        let (low, high) = if range == "*" {
            (minimum, maximum)
        } else if let Some((start, end)) = range.split_once('-') {
            (start.parse::<u32>().ok()?, end.parse::<u32>().ok()?)
        } else {
            let value = range.parse::<u32>().ok()?;
            (value, value)
        };
        if low < minimum || high > maximum || low > high {
            return None;
        }
        let mut value = low;
        while value <= high {
            values.push(value);
            value += step;
        }
    }
    values.sort_unstable();
    values.dedup();
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

pub fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first = NaiveDate::from_ymd_opt(next_year, next_month, 1).unwrap_or_default();
    let this = NaiveDate::from_ymd_opt(year, month, 1).unwrap_or_default();
    (first - this).num_days() as u32
}

#[allow(dead_code)]
pub fn start_of_day(value: DateTime<Utc>) -> DateTime<Utc> {
    let date = value.date_naive();
    date.and_hms_opt(0, 0, 0)
        .map(|naive| Utc.from_utc_datetime(&naive))
        .unwrap_or(value)
}

pub fn start_of_month(value: DateTime<Utc>) -> DateTime<Utc> {
    NaiveDate::from_ymd_opt(value.year(), value.month(), 1)
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|naive| Utc.from_utc_datetime(&naive))
        .unwrap_or(value)
}

#[allow(dead_code)]
pub fn minutes_between(later: DateTime<Utc>, earlier: DateTime<Utc>) -> i64 {
    (later - earlier).num_minutes()
}

#[allow(dead_code)]
pub fn add_minutes(value: DateTime<Utc>, minutes: i64) -> DateTime<Utc> {
    value + Duration::minutes(minutes)
}

#[allow(dead_code)]
pub fn hour_of(value: DateTime<Utc>) -> u32 {
    value.hour()
}
