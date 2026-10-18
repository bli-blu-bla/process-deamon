//! Cron-style schedules for process.toml: classic 5-field expressions
//! ("*/15 * * * *"), the usual macros (@hourly, @daily, ...) and fixed
//! intervals ("@every 30m"). Evaluated in local time (TZ / /etc/localtime).

use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike};

use crate::duration;

/// Never look further ahead than this for a match (covers "29 2 29 2 *" -
/// Feb 29th - plus the leap-year gap around 2100).
const MAX_SEARCH_YEARS: i32 = 9;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    source: String,
    kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Cron(Cron),
    Every(Duration),
}

/// One bitset per field; bit n set = value n matches.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cron {
    minutes: u64,
    hours: u32,
    days: u32,   // 1..=31
    months: u16, // 1..=12
    weekdays: u8, // 0..=6, Sunday = 0
    /// Standard cron quirk: when both day-of-month and day-of-week are
    /// restricted, a day matches if EITHER does.
    days_restricted: bool,
    weekdays_restricted: bool,
}

/// What a schedule does when it fires.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Action {
    /// Start the process unless it is already running (cron job style).
    #[default]
    Start,
    /// Stop the process if it is running, then start it again.
    Restart,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Start => "start",
            Action::Restart => "restart",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "start" => Ok(Action::Start),
            "restart" => Ok(Action::Restart),
            _ => Err(format!("invalid schedule_action {s:?} (expected \"start\" or \"restart\")")),
        }
    }
}

impl fmt::Display for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

impl Schedule {
    pub fn parse(input: &str) -> Result<Self, String> {
        let source = input.trim().to_owned();
        let kind = match source.as_str() {
            "@yearly" | "@annually" => Kind::Cron(Cron::parse("0 0 1 1 *")?),
            "@monthly" => Kind::Cron(Cron::parse("0 0 1 * *")?),
            "@weekly" => Kind::Cron(Cron::parse("0 0 * * 0")?),
            "@daily" | "@midnight" => Kind::Cron(Cron::parse("0 0 * * *")?),
            "@hourly" => Kind::Cron(Cron::parse("0 * * * *")?),
            s if s.starts_with("@every") => {
                let arg = s["@every".len()..].trim();
                let every = duration::parse(arg).map_err(|e| format!("invalid schedule {input:?}: {e}"))?;
                if every < Duration::from_secs(1) {
                    return Err(format!("invalid schedule {input:?}: interval must be at least 1s"));
                }
                Kind::Every(every)
            }
            s if s.starts_with('@') => return Err(format!("invalid schedule {input:?}: unknown macro")),
            s => Kind::Cron(Cron::parse(s).map_err(|e| format!("invalid schedule {input:?}: {e}"))?),
        };
        Ok(Self { source, kind })
    }

    /// The first fire time strictly after `after`, or None if the expression
    /// can never match (e.g. "0 0 31 2 *").
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        match &self.kind {
            Kind::Every(every) => Some(after + ChronoDuration::from_std(*every).ok()?),
            Kind::Cron(cron) => cron.next_after(after),
        }
    }
}

impl Cron {
    fn parse(expr: &str) -> Result<Self, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        let [minute, hour, dom, month, dow] = fields[..] else {
            return Err(format!("expected 5 fields (minute hour day month weekday), got {}", fields.len()));
        };
        let weekdays = parse_field(dow, 0, 7, &WEEKDAY_NAMES).map_err(|e| format!("weekday: {e}"))?;
        Ok(Self {
            minutes: parse_field(minute, 0, 59, &[]).map_err(|e| format!("minute: {e}"))?,
            hours: parse_field(hour, 0, 23, &[]).map_err(|e| format!("hour: {e}"))? as u32,
            days: parse_field(dom, 1, 31, &[]).map_err(|e| format!("day: {e}"))? as u32,
            months: parse_field(month, 1, 12, &MONTH_NAMES).map_err(|e| format!("month: {e}"))? as u16,
            // 7 is an alias for Sunday.
            weekdays: ((weekdays | (weekdays >> 7)) & 0x7f) as u8,
            days_restricted: !dom.starts_with('*'),
            weekdays_restricted: !dow.starts_with('*'),
        })
    }

    fn day_matches(&self, date: NaiveDate) -> bool {
        let dom = self.days & (1 << date.day()) != 0;
        let dow = self.weekdays & (1 << date.weekday().num_days_from_sunday()) != 0;
        match (self.days_restricted, self.weekdays_restricted) {
            (true, true) => dom || dow,
            _ => dom && dow,
        }
    }

    fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        let start = after.naive_local().with_second(0)?.with_nanosecond(0)? + ChronoDuration::minutes(1);
        let limit = start.year() + MAX_SEARCH_YEARS;
        let mut t = start;
        while t.year() <= limit {
            if self.months & (1 << t.month()) == 0 {
                t = first_of_next_month(t)?;
                continue;
            }
            if !self.day_matches(t.date()) {
                t = t.date().succ_opt()?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if self.hours & (1 << t.hour()) == 0 {
                t = t.with_minute(0)? + ChronoDuration::hours(1);
                continue;
            }
            if self.minutes & (1 << t.minute()) == 0 {
                t += ChronoDuration::minutes(1);
                continue;
            }
            // Ambiguous local time (DST fall back): fire once, on the first
            // occurrence. Skipped local time (spring forward): doesn't exist.
            match Local.from_local_datetime(&t).earliest() {
                Some(dt) if dt > after => return Some(dt),
                _ => t += ChronoDuration::minutes(1),
            }
        }
        None
    }
}

fn first_of_next_month(t: NaiveDateTime) -> Option<NaiveDateTime> {
    let (y, m) = if t.month() == 12 { (t.year() + 1, 1) } else { (t.year(), t.month() + 1) };
    NaiveDate::from_ymd_opt(y, m, 1)?.and_hms_opt(0, 0, 0)
}

const MONTH_NAMES: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const WEEKDAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// Parses one field: "*", "5", "1-5", "*/15", "10-50/10", "mon-fri", and
/// comma-separated lists of those. `names[i]` stands for `min + i`.
fn parse_field(field: &str, min: u32, max: u32, names: &[&str]) -> Result<u64, String> {
    let value = |s: &str| -> Result<u32, String> {
        let lower = s.to_ascii_lowercase();
        if let Some(i) = names.iter().position(|n| *n == lower) {
            return Ok(min + i as u32);
        }
        let v: u32 = s.parse().map_err(|_| format!("invalid value {s:?}"))?;
        if v < min || v > max {
            return Err(format!("{v} out of range {min}-{max}"));
        }
        Ok(v)
    };

    let mut bits = 0u64;
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step.parse().map_err(|_| format!("invalid step {step:?}"))?;
                if step == 0 {
                    return Err("step must not be 0".into());
                }
                (range, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = match range {
            "*" => (min, max),
            r => match r.split_once('-') {
                Some((lo, hi)) => (value(lo)?, value(hi)?),
                // "5/10" means "from 5 to the end, every 10", as in Vixie cron.
                None if step > 1 => (value(r)?, max),
                None => {
                    let v = value(r)?;
                    (v, v)
                }
            },
        };
        if lo > hi {
            return Err(format!("invalid range {range:?}"));
        }
        for v in (lo..=hi).step_by(step as usize) {
            bits |= 1 << v;
        }
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Local> {
        let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
        Local.from_local_datetime(&naive).earliest().unwrap()
    }

    fn next(expr: &str, after: &str) -> String {
        let s = Schedule::parse(expr).unwrap();
        s.next_after(at(after)).unwrap().format("%Y-%m-%d %H:%M:%S").to_string()
    }

    #[test]
    fn computes_next_fire_time() {
        assert_eq!(next("* * * * *", "2026-01-01 10:00:30"), "2026-01-01 10:01:00");
        assert_eq!(next("*/15 * * * *", "2026-01-01 10:00:00"), "2026-01-01 10:15:00");
        assert_eq!(next("0 3 * * *", "2026-01-01 03:00:00"), "2026-01-02 03:00:00");
        assert_eq!(next("30 4 1 * *", "2026-01-31 12:00:00"), "2026-02-01 04:30:00");
        assert_eq!(next("0 9 * * mon-fri", "2026-10-17 12:00:00"), "2026-10-19 09:00:00"); // Sat -> Mon
        assert_eq!(next("0 0 * * 7", "2026-10-17 12:00:00"), "2026-10-18 00:00:00"); // 7 = Sunday
        assert_eq!(next("0 0 29 feb *", "2026-03-01 00:00:00"), "2028-02-29 00:00:00");
        assert_eq!(next("@daily", "2026-12-31 23:59:00"), "2027-01-01 00:00:00");
        assert_eq!(next("@hourly", "2026-01-01 10:59:59"), "2026-01-01 11:00:00");
        assert_eq!(next("5,10-20/5 * * * *", "2026-01-01 10:06:00"), "2026-01-01 10:10:00");
        assert_eq!(next("@every 90s", "2026-01-01 10:00:00"), "2026-01-01 10:01:30");
    }

    #[test]
    fn day_of_month_or_weekday_when_both_restricted() {
        // The 13th OR any Friday: 2026-10-16 is a Friday, before the 13th of November.
        assert_eq!(next("0 0 13 * fri", "2026-10-14 00:00:00"), "2026-10-16 00:00:00");
    }

    #[test]
    fn impossible_dates_never_fire() {
        assert!(Schedule::parse("0 0 31 2 *").unwrap().next_after(at("2026-01-01 00:00:00")).is_none());
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "* * * *", "60 * * * *", "* 24 * * *", "* * 0 * *", "*/0 * * * *", "5-1 * * * *", "@sometimes", "@every 10", "@every 0s", "* * * foo *"] {
            assert!(Schedule::parse(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(Action::parse("reload").is_err());
    }
}
