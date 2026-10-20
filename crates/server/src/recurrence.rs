//! Recurrence expansion for recurring meeting series (issue 16).
//!
//! Deliberately hand-rolled for the curated frequency set the product
//! offers (daily / weekly / every weekday / monthly). A general RRULE
//! engine would drag in a full RFC 5545 parser and timezone machinery
//! for rules this system can never produce, while the join window, the
//! dashboard, and the ICS export all need exactly one operation:
//! expand a series start into concrete occurrence start times.

use huddle_protocol::{RecurrenceRule, RepeatFrequency};
use time::macros::format_description;
use time::{Duration, Month, OffsetDateTime, Weekday};

/// Hard cap on expanded occurrences for open-ended series. Expansion
/// runs per join / dashboard request, so an unbounded rule must not
/// expand unboundedly; a thousand occurrences is far beyond any
/// horizon the UI shows.
pub const MAX_OCCURRENCES: usize = 1000;

/// The persisted form of a frequency: an RFC 5545 `FREQ` value, plus
/// `BYDAY` for the weekday rule. A closed set by design — anything
/// else in the column was written by another build and is refused
/// rather than reinterpreted (see [`parse_stored_rule`]).
pub fn stored_rule(frequency: RepeatFrequency) -> &'static str {
    match frequency {
        RepeatFrequency::Daily => "FREQ=DAILY",
        RepeatFrequency::Weekly => "FREQ=WEEKLY",
        RepeatFrequency::Weekdays => "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR",
        RepeatFrequency::Monthly => "FREQ=MONTHLY",
    }
}

/// Inverse of [`stored_rule`], exact-match only: the database stores
/// exactly what we wrote, or the row is corrupt — never something to
/// guess at.
pub fn parse_stored_rule(raw: &str) -> Option<RepeatFrequency> {
    match raw {
        "FREQ=DAILY" => Some(RepeatFrequency::Daily),
        "FREQ=WEEKLY" => Some(RepeatFrequency::Weekly),
        "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR" => Some(RepeatFrequency::Weekdays),
        "FREQ=MONTHLY" => Some(RepeatFrequency::Monthly),
        _ => None,
    }
}

/// An RFC 5545 `DATE-TIME` in UTC (`20260601T090000Z`) — the form both
/// `UNTIL` values and `EXDATE` properties need.
pub fn ics_datetime(t: OffsetDateTime) -> Result<String, time::error::Format> {
    let fmt = format_description!("[year][month][day]T[hour][minute][second]Z");
    // The `Z` suffix is only honest once the wall clock is UTC.
    t.replace_offset(time::UtcOffset::UTC).format(&fmt)
}

/// The `RRULE` property value for a rule: the frequency fragment plus
/// `COUNT` or `UNTIL`. RFC 5545 §3.3.10 makes those parts of the rule
/// value itself — they are never separate properties.
pub fn rrule_property(rule: &RecurrenceRule) -> Result<String, time::error::Format> {
    let mut value = stored_rule(rule.frequency).to_owned();
    if let Some(until) = rule.until {
        value.push_str(";UNTIL=");
        value.push_str(&ics_datetime(until)?);
    } else if let Some(count) = rule.count {
        value.push_str(";COUNT=");
        value.push_str(&count.to_string());
    }
    Ok(value)
}

/// Does `candidate` count as an occurrence under `frequency`? Only the
/// weekday rule can miss — weekends never occur.
fn matches(frequency: RepeatFrequency, candidate: OffsetDateTime) -> bool {
    match frequency {
        RepeatFrequency::Weekdays => {
            !matches!(candidate.weekday(), Weekday::Saturday | Weekday::Sunday)
        }
        RepeatFrequency::Daily | RepeatFrequency::Weekly | RepeatFrequency::Monthly => true,
    }
}

/// Step `candidate` forward to the next possible occurrence date of
/// `frequency`.
fn advance(frequency: RepeatFrequency, candidate: OffsetDateTime) -> OffsetDateTime {
    match frequency {
        RepeatFrequency::Daily | RepeatFrequency::Weekdays => candidate + Duration::days(1),
        RepeatFrequency::Weekly => candidate + Duration::days(7),
        RepeatFrequency::Monthly => next_month_same_day(candidate),
    }
}

/// One calendar month later, keeping the day-of-month. Months that
/// lack the day (Jan 31 → February) are skipped entirely: a series
/// starting Jan 31 next occurs Mar 31, which is also how calendar
/// clients interpret a plain `FREQ=MONTHLY` export.
fn next_month_same_day(candidate: OffsetDateTime) -> OffsetDateTime {
    let day = candidate.day();
    let (mut year, mut month) = (candidate.year(), candidate.month());
    loop {
        if month == Month::December {
            month = Month::January;
            year += 1;
        } else {
            month = month.next();
        }
        if let Ok(date) = time::Date::from_calendar_date(year, month, day) {
            return date.with_time(candidate.time()).assume_utc();
        }
        // This month lacks the day-of-month (e.g. Feb 31); try the next.
    }
}

/// Expand a series into its concrete occurrence start times,
/// ascending. `series_start` is the first occurrence when it matches
/// the frequency; a weekday series that starts on a weekend first
/// occurs the following Monday. `until` is inclusive (the occurrence
/// on that timestamp exists), `count` counts every occurrence, and
/// open-ended series are capped at [`MAX_OCCURRENCES`].
pub fn occurrence_starts(
    rule: &RecurrenceRule,
    series_start: OffsetDateTime,
) -> Vec<OffsetDateTime> {
    let mut starts = Vec::new();
    let mut candidate = series_start;
    let until = rule.until;
    loop {
        if matches(rule.frequency, candidate) {
            if let Some(until) = until
                && candidate > until
            {
                break;
            }
            starts.push(candidate);
            if let Some(count) = rule.count
                && starts.len() >= count as usize
            {
                break;
            }
            if starts.len() >= MAX_OCCURRENCES {
                break;
            }
        }
        candidate = advance(rule.frequency, candidate);
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(frequency: RepeatFrequency) -> RecurrenceRule {
        RecurrenceRule {
            frequency,
            until: None,
            count: None,
        }
    }

    fn at(year: i32, month: Month, day: u8, hour: u8) -> OffsetDateTime {
        let date = time::Date::from_calendar_date(year, month, day).unwrap();
        date.with_time(time::Time::from_hms(hour, 0, 0).unwrap())
            .assume_utc()
    }

    #[test]
    fn stored_rules_round_trip_exactly() {
        for frequency in [
            RepeatFrequency::Daily,
            RepeatFrequency::Weekly,
            RepeatFrequency::Weekdays,
            RepeatFrequency::Monthly,
        ] {
            assert_eq!(parse_stored_rule(stored_rule(frequency)), Some(frequency));
        }
        // Anything the closed set does not contain is corrupt, not a
        // rule to reinterpret.
        for rejected in ["FREQ=YEARLY", "FREQ=WEEKLY;INTERVAL=2", ""] {
            assert_eq!(parse_stored_rule(rejected), None);
        }
    }

    #[test]
    fn daily_weekly_and_monthly_expand_from_the_series_start() {
        let start = at(2026, Month::January, 1, 9);
        let daily = occurrence_starts(&rule(RepeatFrequency::Daily), start);
        assert_eq!(daily.len(), MAX_OCCURRENCES, "open-ended is capped");
        assert_eq!(daily[1], start + Duration::days(1));

        let weekly = occurrence_starts(&rule(RepeatFrequency::Weekly), start);
        assert_eq!(weekly[1], start + Duration::days(7));

        let monthly = occurrence_starts(&rule(RepeatFrequency::Monthly), start);
        assert_eq!(monthly[1], at(2026, Month::February, 1, 9));
        assert_eq!(monthly[12], at(2027, Month::January, 1, 9));
    }

    #[test]
    fn monthly_skips_months_that_lack_the_day() {
        // Jan 31 → no Feb 31 → Mar 31; no Apr 31 → May 31.
        let start = at(2026, Month::January, 31, 9);
        let monthly = occurrence_starts(&rule(RepeatFrequency::Monthly), start);
        assert_eq!(monthly[0], start);
        assert_eq!(monthly[1], at(2026, Month::March, 31, 9));
        assert_eq!(monthly[2], at(2026, Month::May, 31, 9));
    }

    #[test]
    fn weekdays_skip_weekends_even_when_the_series_starts_on_one() {
        // 2026-01-03 is a Saturday: the first occurrence is Monday.
        let start = at(2026, Month::January, 3, 9);
        let mut weekdays = rule(RepeatFrequency::Weekdays);
        weekdays.count = Some(5);
        let starts = occurrence_starts(&weekdays, start);
        assert_eq!(
            starts,
            vec![
                at(2026, Month::January, 5, 9),
                at(2026, Month::January, 6, 9),
                at(2026, Month::January, 7, 9),
                at(2026, Month::January, 8, 9),
                at(2026, Month::January, 9, 9),
            ]
        );
    }

    #[test]
    fn until_is_inclusive_and_count_counts_every_occurrence() {
        let start = at(2026, Month::January, 1, 9);
        let mut bounded = rule(RepeatFrequency::Daily);
        bounded.until = Some(start + Duration::days(2));
        let starts = occurrence_starts(&bounded, start);
        assert_eq!(
            starts,
            vec![start, start + Duration::days(1), start + Duration::days(2),]
        );

        let mut counted = rule(RepeatFrequency::Daily);
        counted.count = Some(3);
        assert_eq!(occurrence_starts(&counted, start).len(), 3);
    }

    #[test]
    fn rrule_property_is_rfc_5545_shaped() {
        assert_eq!(
            rrule_property(&rule(RepeatFrequency::Weekly)).unwrap(),
            "FREQ=WEEKLY"
        );
        let mut counted = rule(RepeatFrequency::Weekdays);
        counted.count = Some(10);
        assert_eq!(
            rrule_property(&counted).unwrap(),
            "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;COUNT=10"
        );
        let mut bounded = rule(RepeatFrequency::Monthly);
        bounded.until = Some(at(2026, Month::June, 1, 9));
        assert_eq!(
            rrule_property(&bounded).unwrap(),
            "FREQ=MONTHLY;UNTIL=20260601T090000Z"
        );
    }
}
