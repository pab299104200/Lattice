use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// UTC timezone marker used by the event model timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Utc;

/// Minimal UTC timestamp with serde support and deterministic RFC3339 formatting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DateTime<Tz> {
    unix_seconds: i64,
    _timezone: PhantomData<Tz>,
}

impl Utc {
    pub fn now() -> DateTime<Utc> {
        let unix_seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .min(i64::MAX as u64) as i64;
        DateTime::from_unix_seconds(unix_seconds)
    }
}

impl<Tz> DateTime<Tz> {
    /// Construct a timestamp from whole Unix seconds.
    pub fn from_unix_seconds(unix_seconds: i64) -> Self {
        Self {
            unix_seconds,
            _timezone: PhantomData,
        }
    }

    /// Return the stored Unix timestamp in seconds.
    pub fn unix_seconds(self) -> i64 {
        self.unix_seconds
    }

    pub fn timestamp(self) -> i64 {
        self.unix_seconds
    }
}

impl DateTime<Utc> {
    /// Parse a UTC RFC3339 timestamp in `YYYY-MM-DDTHH:MM:SSZ` form.
    pub fn parse_rfc3339(value: &str) -> Result<Self, String> {
        if value.len() != 20 || !value.ends_with('Z') {
            return Err("expected UTC RFC3339 timestamp in YYYY-MM-DDTHH:MM:SSZ form".to_string());
        }

        let year = parse_i32(&value[0..4], "year")?;
        let month = parse_u32(&value[5..7], "month")?;
        let day = parse_u32(&value[8..10], "day")?;
        let hour = parse_u32(&value[11..13], "hour")?;
        let minute = parse_u32(&value[14..16], "minute")?;
        let second = parse_u32(&value[17..19], "second")?;

        ensure_delimiter(value.as_bytes()[4], '-', "year-month")?;
        ensure_delimiter(value.as_bytes()[7], '-', "month-day")?;
        ensure_delimiter(value.as_bytes()[10], 'T', "date-time")?;
        ensure_delimiter(value.as_bytes()[13], ':', "hour-minute")?;
        ensure_delimiter(value.as_bytes()[16], ':', "minute-second")?;

        if !(1..=12).contains(&month) {
            return Err(format!("month `{month}` is out of range"));
        }
        if !(1..=days_in_month(year, month)).contains(&day) {
            return Err(format!(
                "day `{day}` is out of range for {year:04}-{month:02}"
            ));
        }
        if hour > 23 {
            return Err(format!("hour `{hour}` is out of range"));
        }
        if minute > 59 {
            return Err(format!("minute `{minute}` is out of range"));
        }
        if second > 59 {
            return Err(format!("second `{second}` is out of range"));
        }

        let days = days_from_civil(year, month, day);
        let seconds =
            days * 86_400 + i64::from(hour) * 3_600 + i64::from(minute) * 60 + i64::from(second);
        Ok(Self::from_unix_seconds(seconds))
    }

    fn to_rfc3339(self) -> String {
        let days = self.unix_seconds.div_euclid(86_400);
        let seconds_of_day = self.unix_seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let hour = seconds_of_day / 3_600;
        let minute = (seconds_of_day % 3_600) / 60;
        let second = seconds_of_day % 60;

        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    }
}

impl Serialize for DateTime<Utc> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_rfc3339())
    }
}

impl<'de> Deserialize<'de> for DateTime<Utc> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DateTimeVisitor;

        impl<'de> Visitor<'de> for DateTimeVisitor {
            type Value = DateTime<Utc>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a UTC RFC3339 timestamp in YYYY-MM-DDTHH:MM:SSZ form")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                DateTime::<Utc>::parse_rfc3339(value).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                DateTime::<Utc>::parse_rfc3339(&value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(DateTimeVisitor)
    }
}

fn parse_i32(value: &str, field: &str) -> Result<i32, String> {
    value
        .parse::<i32>()
        .map_err(|_| format!("invalid {field} component `{value}`"))
}

fn parse_u32(value: &str, field: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|_| format!("invalid {field} component `{value}`"))
}

fn ensure_delimiter(actual: u8, expected: char, position: &str) -> Result<(), String> {
    if char::from(actual) == expected {
        Ok(())
    } else {
        Err(format!("expected `{expected}` delimiter at {position}"))
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let mut year = i64::from(year);
    let month = i64::from(month);
    let day = i64::from(day);
    year -= if month <= 2 { 1 } else { 0 };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_of_year = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_of_year + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}
