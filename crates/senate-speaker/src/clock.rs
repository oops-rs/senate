//! A committee's day: dates and times of day in its timezone.

use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use tracing::warn;

/// Where a committee's people live: a named IANA zone, or the server's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Local,
    Named(Tz),
}

impl Zone {
    /// `name` as an IANA zone; the server's local zone (with a warning)
    /// when it is not one.
    pub fn parse(name: &str) -> Self {
        match name.trim().parse::<Tz>() {
            Ok(tz) => Self::Named(tz),
            Err(_) => {
                warn!(
                    timezone = name,
                    "not an IANA timezone; using the server's local zone"
                );
                Self::Local
            }
        }
    }

    /// The date `ts` falls on here.
    pub fn date_of(self, ts: DateTime<Utc>) -> NaiveDate {
        match self {
            Self::Local => ts.with_timezone(&Local).date_naive(),
            Self::Named(tz) => ts.with_timezone(&tz).date_naive(),
        }
    }

    /// Today here.
    pub fn today(self) -> NaiveDate {
        self.date_of(Utc::now())
    }

    /// The instant `date` at `time` here. A time skipped by a clock change
    /// is taken an hour later; an ambiguous one at its first occurrence.
    fn at(self, date: NaiveDate, time: NaiveTime) -> Option<DateTime<Utc>> {
        let naive = date.and_time(time);
        let pick = |result: chrono::LocalResult<DateTime<Utc>>| result.earliest();
        let shifted = naive + chrono::Duration::hours(1);
        match self {
            Self::Local => pick(Local.from_local_datetime(&naive).map(|t| t.to_utc()))
                .or_else(|| pick(Local.from_local_datetime(&shifted).map(|t| t.to_utc()))),
            Self::Named(tz) => pick(tz.from_local_datetime(&naive).map(|t| t.to_utc()))
                .or_else(|| pick(tz.from_local_datetime(&shifted).map(|t| t.to_utc()))),
        }
    }

    /// The next time it is `time` here, strictly after `now`.
    pub fn next(self, now: DateTime<Utc>, time: NaiveTime) -> DateTime<Utc> {
        let today = self.date_of(now);
        let mut date = today;
        for _ in 0..3 {
            if let Some(at) = self.at(date, time)
                && at > now
            {
                return at;
            }
            date = date.succ_opt().unwrap_or(date);
        }
        now + chrono::Duration::days(1)
    }

    /// How long from `now` until the next `time` here.
    pub fn until(self, now: DateTime<Utc>, time: NaiveTime) -> Duration {
        (self.next(now, time) - now)
            .to_std()
            .unwrap_or(Duration::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn hm(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn the_next_time_of_day_is_in_the_committees_zone() {
        let zone = Zone::parse("Asia/Tokyo"); // UTC+9, no clock changes
        // 08:00 UTC is 17:00 in Tokyo: 18:00 is an hour away.
        assert_eq!(
            zone.until(utc("2026-11-20T08:00:00Z"), hm(18, 0)),
            Duration::from_secs(3_600)
        );
        // 10:00 UTC is 19:00 there: tomorrow's 18:00.
        assert_eq!(
            zone.next(utc("2026-11-20T10:00:00Z"), hm(18, 0)),
            utc("2026-11-21T09:00:00Z")
        );
        // Exactly at the time: the next day's.
        assert_eq!(
            zone.next(utc("2026-11-20T09:00:00Z"), hm(18, 0)),
            utc("2026-11-21T09:00:00Z")
        );
        assert_eq!(
            zone.date_of(utc("2026-11-20T16:00:00Z")),
            NaiveDate::from_ymd_opt(2026, 11, 21).unwrap()
        );
    }

    #[test]
    fn a_time_a_clock_change_skips_is_taken_an_hour_later() {
        // Berlin skips 02:00–03:00 on 2026-03-29.
        let zone = Zone::parse("Europe/Berlin");
        assert_eq!(
            zone.next(utc("2026-03-28T12:00:00Z"), hm(2, 30)),
            utc("2026-03-29T01:30:00Z")
        );
    }

    #[test]
    fn an_unknown_zone_is_the_servers_own() {
        assert_eq!(Zone::parse("Mars/Olympus"), Zone::Local);
    }
}
