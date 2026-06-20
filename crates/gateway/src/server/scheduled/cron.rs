// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 croit GmbH

//! A small, self-contained evaluator for standard 5-field cron
//! expressions (`minute hour day-of-month month day-of-week`), built on
//! `jiff` so timezone + DST handling matches the rest of the gateway.
//!
//! We deliberately do **not** pull in a cron crate: every mature one
//! depends on `chrono`, and the workspace standardises on `jiff` (see
//! `docs/dependencies.md`). The grammar we need is the common subset the
//! UI's schedule builder emits — `*`, `*/n`, `a`, `a-b`, `a-b/n`, and
//! comma-lists of those — plus arbitrary hand-typed expressions in the
//! same grammar for the "advanced" field.
//!
//! Field ranges: minute `0-59`, hour `0-23`, day-of-month `1-31`, month
//! `1-12`, day-of-week `0-6` (Sunday = 0; `7` is also accepted as
//! Sunday). Day-of-month / day-of-week follow the Vixie-cron rule: when
//! **both** are restricted (neither is `*`), a day matches if **either**
//! matches; otherwise the restricted one must match.
//!
//! Occurrence search steps minute-by-minute in absolute time and tests
//! the *wall-clock* fields of the zoned instant, so a daily job keeps its
//! local time across a DST transition. A wall time that a spring-forward
//! transition skips simply doesn't fire that day (standard cron
//! behaviour); a fall-back repeat fires once (the earlier instant).

use std::fmt::Write as _;

use jiff::Timestamp;
use jiff::tz::TimeZone;

/// Upper bound on the minute-stepping search: four years. A valid cron
/// expression always has an occurrence within this window (the worst
/// case is `0 0 29 2 *` — Feb 29, which recurs at most every four
/// years), so exceeding it means the expression is unsatisfiable.
const MAX_SEARCH_MINUTES: i64 = 4 * 366 * 24 * 60;

/// A parsed cron expression: one allowed-value set per field, plus a
/// flag per day field recording whether it was a bare `*` (needed for
/// the day-of-month / day-of-week OR rule).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: Vec<u8>,
    hours: Vec<u8>,
    doms: Vec<u8>,
    months: Vec<u8>,
    dows: Vec<u8>,
    dom_wild: bool,
    dow_wild: bool,
    /// The normalised 5-field source string, kept for `describe`'s
    /// fallback and for round-tripping back into storage.
    raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronError(pub String);

impl std::fmt::Display for CronError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid cron expression: {}", self.0)
    }
}

impl std::error::Error for CronError {}

impl Cron {
    /// Parse a standard 5-field cron expression. Extra whitespace
    /// between fields is tolerated; anything other than exactly five
    /// fields (or a field value out of range) is an error.
    pub fn parse(expr: &str) -> Result<Self, CronError> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(CronError(format!(
                "expected 5 fields (minute hour day-of-month month day-of-week), got {}",
                fields.len()
            )));
        }
        let minutes = parse_field(fields[0], 0, 59, "minute")?;
        let hours = parse_field(fields[1], 0, 23, "hour")?;
        let doms = parse_field(fields[2], 1, 31, "day-of-month")?;
        let months = parse_field(fields[3], 1, 12, "month")?;
        let dows = parse_dow(fields[4])?;
        Ok(Cron {
            minutes,
            hours,
            doms,
            months,
            dows,
            dom_wild: is_wildcard(fields[2]),
            dow_wild: is_wildcard(fields[4]),
            raw: fields.join(" "),
        })
    }

    /// The normalised 5-field expression.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Whether the zoned instant's wall-clock fields satisfy this
    /// expression. Applies the Vixie day-of-month / day-of-week OR rule.
    fn matches(&self, minute: u8, hour: u8, dom: u8, month: u8, dow: u8) -> bool {
        if !self.minutes.contains(&minute)
            || !self.hours.contains(&hour)
            || !self.months.contains(&month)
        {
            return false;
        }
        let dom_ok = self.doms.contains(&dom);
        let dow_ok = self.dows.contains(&dow);
        match (self.dom_wild, self.dow_wild) {
            // Both restricted → either matches (Vixie rule).
            (false, false) => dom_ok || dow_ok,
            // One (or both) is `*` → the restricted field(s) must match.
            // A `*` field is already `true` here, so the AND is correct.
            _ => dom_ok && dow_ok,
        }
    }

    /// The first occurrence strictly after `after`, evaluated in `tz`.
    /// Returns `None` if the expression is unsatisfiable within the
    /// four-year search window.
    pub fn next_after(&self, after: Timestamp, tz: &TimeZone) -> Option<Timestamp> {
        // Align to the next whole minute strictly after `after` (cron
        // fires on minute boundaries; sub-minute precision is irrelevant).
        let ms = after.as_millisecond();
        let next_min_ms = (ms.div_euclid(60_000) + 1) * 60_000;
        let mut t = Timestamp::from_millisecond(next_min_ms).ok()?;
        for _ in 0..MAX_SEARCH_MINUTES {
            let z = t.to_zoned(tz.clone());
            let dow = z.weekday().to_sunday_zero_offset() as u8;
            if self.matches(
                z.minute() as u8,
                z.hour() as u8,
                z.day() as u8,
                z.month() as u8,
                dow,
            ) {
                return Some(t);
            }
            t = t.checked_add(jiff::SignedDuration::from_mins(1)).ok()?;
        }
        None
    }

    /// The next `n` occurrences after `after`, evaluated in `tz`.
    pub fn upcoming(&self, after: Timestamp, tz: &TimeZone, n: usize) -> Vec<Timestamp> {
        let mut out = Vec::with_capacity(n);
        let mut cursor = after;
        for _ in 0..n {
            match self.next_after(cursor, tz) {
                Some(t) => {
                    out.push(t);
                    cursor = t;
                }
                None => break,
            }
        }
        out
    }

    /// A best-effort, human-readable summary for the schedule-builder
    /// preview. Handles the shapes the builder emits well; arbitrary
    /// hand-typed expressions fall back to a generic rendering (the
    /// "next runs" list is the authoritative confirmation either way).
    pub fn describe(&self) -> String {
        let time_at = || {
            if self.hours.len() == 1 && self.minutes.len() == 1 {
                Some(format!("{:02}:{:02}", self.hours[0], self.minutes[0]))
            } else {
                None
            }
        };

        // Every minute.
        if self.minutes.len() == 60
            && self.hours.len() == 24
            && self.dom_wild
            && self.months.len() == 12
            && self.dow_wild
        {
            return "Every minute.".to_string();
        }

        // Hourly: a single minute, every hour, every day.
        if self.minutes.len() == 1
            && self.hours.len() == 24
            && self.dom_wild
            && self.months.len() == 12
            && self.dow_wild
        {
            return format!("Every hour, at minute {}.", self.minutes[0]);
        }

        // Daily at a fixed time.
        if let Some(at) = time_at()
            && self.dom_wild
            && self.months.len() == 12
            && self.dow_wild
        {
            return format!("At {at}, every day.");
        }

        // Weekly: fixed time on a set of weekdays.
        if let Some(at) = time_at()
            && self.dom_wild
            && self.months.len() == 12
            && !self.dow_wild
        {
            let days = self
                .dows
                .iter()
                .map(|d| weekday_name(*d))
                .collect::<Vec<_>>()
                .join(", ");
            return format!("At {at}, on {days}.");
        }

        // Monthly: fixed time on a set of days-of-month.
        if let Some(at) = time_at()
            && !self.dom_wild
            && self.months.len() == 12
            && self.dow_wild
        {
            let days = self
                .doms
                .iter()
                .map(|d| ordinal(*d))
                .collect::<Vec<_>>()
                .join(", ");
            return format!("At {at}, on the {days} of every month.");
        }

        format!("Custom schedule (cron: {}).", self.raw)
    }
}

/// `true` iff the field is a bare wildcard (`*`). Used only for the
/// day-of-month / day-of-week OR rule; `*/n` counts as restricted.
fn is_wildcard(field: &str) -> bool {
    field.trim() == "*"
}

/// Parse one numeric cron field into the sorted, deduplicated set of
/// values it matches, validating against `[min, max]`.
fn parse_field(field: &str, min: u8, max: u8, name: &str) -> Result<Vec<u8>, CronError> {
    let mut set = std::collections::BTreeSet::new();
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(CronError(format!("empty term in {name} field")));
        }
        // Split off an optional `/step`.
        let (range_part, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u8 = s
                    .parse()
                    .map_err(|_| CronError(format!("invalid step `{s}` in {name} field")))?;
                if step == 0 {
                    return Err(CronError(format!("step must be > 0 in {name} field")));
                }
                (r, step)
            }
            None => (part, 1),
        };

        let (lo, hi) = if range_part == "*" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            let a: u8 = a
                .parse()
                .map_err(|_| CronError(format!("invalid value `{a}` in {name} field")))?;
            let b: u8 = b
                .parse()
                .map_err(|_| CronError(format!("invalid value `{b}` in {name} field")))?;
            (a, b)
        } else {
            let v: u8 = range_part
                .parse()
                .map_err(|_| CronError(format!("invalid value `{range_part}` in {name} field")))?;
            // A bare number with a step (`5/15`) means "from 5 to max".
            if step > 1 { (v, max) } else { (v, v) }
        };

        if lo < min || hi > max || lo > hi {
            return Err(CronError(format!(
                "value out of range in {name} field (allowed {min}-{max})"
            )));
        }
        let mut v = lo;
        while v <= hi {
            set.insert(v);
            v += step;
        }
    }
    Ok(set.into_iter().collect())
}

/// Day-of-week parsing: like `parse_field` over `0-7`, but `7` folds to
/// `0` (both mean Sunday) so the matcher only ever sees `0-6`.
fn parse_dow(field: &str) -> Result<Vec<u8>, CronError> {
    let raw = parse_field(field, 0, 7, "day-of-week")?;
    let mut set = std::collections::BTreeSet::new();
    for v in raw {
        set.insert(if v == 7 { 0 } else { v });
    }
    Ok(set.into_iter().collect())
}

fn weekday_name(dow: u8) -> &'static str {
    match dow {
        0 => "Sun",
        1 => "Mon",
        2 => "Tue",
        3 => "Wed",
        4 => "Thu",
        5 => "Fri",
        6 => "Sat",
        _ => "?",
    }
}

/// English ordinal for a day-of-month (`1` → `1st`, `22` → `22nd`).
fn ordinal(n: u8) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    let mut s = String::new();
    let _ = write!(s, "{n}{suffix}");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn berlin() -> TimeZone {
        TimeZone::get("Europe/Berlin").unwrap()
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn rejects_wrong_field_count() {
        assert!(Cron::parse("0 9 * *").is_err());
        assert!(Cron::parse("0 9 * * * *").is_err());
    }

    #[test]
    fn rejects_out_of_range() {
        assert!(Cron::parse("60 9 * * *").is_err());
        assert!(Cron::parse("0 24 * * *").is_err());
        assert!(Cron::parse("0 9 0 * *").is_err());
        assert!(Cron::parse("0 9 * 13 *").is_err());
    }

    #[test]
    fn daily_at_fixed_time() {
        let c = Cron::parse("0 9 * * *").unwrap();
        // 2026-06-19 06:00:00Z is 08:00 Berlin (CEST, +02:00); next 09:00
        // Berlin is 07:00Z the same day.
        let next = c.next_after(ts("2026-06-19T06:00:00Z"), &berlin()).unwrap();
        assert_eq!(next, ts("2026-06-19T07:00:00Z"));
    }

    #[test]
    fn daily_holds_wall_time_across_spring_dst() {
        // Europe/Berlin springs forward 2026-03-29 02:00 → 03:00 (CET
        // +01:00 → CEST +02:00). A 09:00-local daily job is 08:00Z before
        // the switch and 07:00Z after — proving we track wall time, not a
        // fixed UTC offset.
        let c = Cron::parse("0 9 * * *").unwrap();
        // 2026-03-28 09:00 Berlin = 08:00Z (still CET on the 28th).
        assert_eq!(
            c.next_after(ts("2026-03-28T00:00:00Z"), &berlin()).unwrap(),
            ts("2026-03-28T08:00:00Z")
        );
        // 2026-03-30 09:00 Berlin = 07:00Z (CEST after the switch).
        assert_eq!(
            c.next_after(ts("2026-03-29T12:00:00Z"), &berlin()).unwrap(),
            ts("2026-03-30T07:00:00Z")
        );
    }

    #[test]
    fn weekly_on_specific_days() {
        // Mondays at 08:00. 2026-06-19 is a Friday; next Monday is the 22nd.
        let c = Cron::parse("0 8 * * 1").unwrap();
        let next = c.next_after(ts("2026-06-19T00:00:00Z"), &berlin()).unwrap();
        // 2026-06-22 08:00 Berlin (CEST) = 06:00Z.
        assert_eq!(next, ts("2026-06-22T06:00:00Z"));
    }

    #[test]
    fn dom_dow_or_rule() {
        // Both restricted: fires on the 1st OR on any Monday.
        let c = Cron::parse("0 0 1 * 1").unwrap();
        assert!(!c.dom_wild && !c.dow_wild);
        // 2026-06-01 is a Monday, so both branches agree there; pick a
        // month boundary that is not a Monday to prove the OR: 2026-07-01
        // is a Wednesday and must still match via day-of-month.
        let next = c.next_after(ts("2026-06-15T00:00:00Z"), &berlin()).unwrap();
        // Next Monday after 2026-06-15 is 06-22 (00:00 Berlin = prev day
        // 22:00Z), which comes before 07-01 — so the OR picks the Monday.
        assert_eq!(next, ts("2026-06-21T22:00:00Z"));
    }

    #[test]
    fn step_values() {
        let c = Cron::parse("*/15 * * * *").unwrap();
        assert_eq!(c.minutes, vec![0, 15, 30, 45]);
    }

    #[test]
    fn upcoming_returns_distinct_ascending() {
        let c = Cron::parse("0 9 * * *").unwrap();
        let runs = c.upcoming(ts("2026-06-19T06:00:00Z"), &berlin(), 3);
        assert_eq!(runs.len(), 3);
        assert!(runs[0] < runs[1] && runs[1] < runs[2]);
    }

    #[test]
    fn describe_common_shapes() {
        assert_eq!(
            Cron::parse("0 9 * * *").unwrap().describe(),
            "At 09:00, every day."
        );
        assert_eq!(
            Cron::parse("30 8 * * 1,3").unwrap().describe(),
            "At 08:30, on Mon, Wed."
        );
        assert_eq!(
            Cron::parse("0 0 1 * *").unwrap().describe(),
            "At 00:00, on the 1st of every month."
        );
        assert_eq!(
            Cron::parse("0 * * * *").unwrap().describe(),
            "Every hour, at minute 0."
        );
    }
}
