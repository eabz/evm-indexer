//! Calendar dates in UTC, and nothing else.
//!
//! `--start-date`, the coverage floor and every line the panel and
//! `indexer verify` print are about DAYS, not instants, and the whole
//! project already stores time as unix seconds. This file is the conversion
//! between the two, in both directions, and it is deliberately the only
//! place in the codebase that knows how many days there are in February.
//!
//! **Why not a date crate.** The dependency list is short on purpose and
//! every crate on it is audited. What is needed here is two functions of
//! about ten lines each, both of them standard and both of them tested
//! against the dates that break the naive versions (leap years, century
//! years, the 400-year rule, 1970 itself). A crate would be more code, not
//! less, and it would be code nobody in this repository has read.
//!
//! The algorithm is Howard Hinnant's `days_from_civil` / `civil_from_days`,
//! which is exact for every proleptic Gregorian date and has no branches
//! for leap years at all. `UNIX_SHIFT` is the number of days from the start
//! of the 400-year era that contains 1970 to 1970-01-01 itself.

/// Days from 0000-03-01 (the start of the era the algorithm counts from)
/// to 1970-01-01.
const UNIX_SHIFT: i64 = 719_468;

/// Days in one 400-year Gregorian era: 400 * 365 + 97 leap days.
const DAYS_PER_ERA: i64 = 146_097;

pub const SECONDS_PER_DAY: i64 = 86_400;

/// A calendar day in UTC. The only date type in the codebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub year: i64,
    pub month: u32,
    pub day: u32,
}

impl std::fmt::Display for Date {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl Date {
    /// Midnight UTC of this date, as unix seconds.
    pub fn midnight(self) -> i64 {
        days_from_civil(self.year, self.month, self.day) * SECONDS_PER_DAY
    }

    /// The UTC date a unix timestamp falls on.
    pub fn of(timestamp: i64) -> Self {
        // Integer division truncates towards zero, so a timestamp before
        // 1970 has to floor instead: -1 is still 1969-12-31.
        let days = timestamp.div_euclid(SECONDS_PER_DAY);
        civil_from_days(days)
    }

    /// `YYYY-MM-DD`, the one accepted spelling.
    ///
    /// Strict on purpose. `2026-1-5` and `2026/01/05` are refused rather
    /// than guessed at, because this value fixes a chain's coverage floor
    /// for good and a misread date is not something the owner would notice
    /// until much later.
    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();

        let bad = |why: &str| {
            Err(format!(
                "'{value}' is not a date: {why}. Write it as YYYY-MM-DD, \
                 for example 2024-01-31. It is read as midnight UTC."
            ))
        };

        let parts: Vec<&str> = value.split('-').collect();
        if parts.len() != 3 {
            return bad("it needs a year, a month and a day");
        }
        if parts[0].len() != 4
            || parts[1].len() != 2
            || parts[2].len() != 2
        {
            return bad(
                "the year is 4 digits and the month and day are 2",
            );
        }
        if !parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit())) {
            return bad("it may only contain digits and dashes");
        }

        let year: i64 = parts[0].parse().map_err(|_| String::new())?;
        let month: u32 = parts[1].parse().map_err(|_| String::new())?;
        let day: u32 = parts[2].parse().map_err(|_| String::new())?;

        if !(1..=12).contains(&month) {
            return bad("there is no such month");
        }
        if day < 1 || day > days_in_month(year, month) {
            return bad("there is no such day in that month");
        }
        // No blockchain predates this, and a year like 0002 is a typo for
        // something.
        if !(1970..=9999).contains(&year) {
            return bad("the year is outside 1970-9999");
        }

        Ok(Self { year, month, day })
    }
}

/// Midnight UTC of the day a timestamp falls on. What the aggregates mean
/// by "a bucket" and what the floor is reported against.
pub fn start_of_day(timestamp: i64) -> i64 {
    timestamp.div_euclid(SECONDS_PER_DAY) * SECONDS_PER_DAY
}

/// `YYYY-MM-DD` of a unix timestamp, for a log line or a web page.
pub fn format(timestamp: i64) -> String {
    Date::of(timestamp).to_string()
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    // March is treated as the first month, which is what removes every
    // leap-year branch: the leap day then lands at the END of the year.
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;

    let shifted = if month > 2 { month - 3 } else { month + 9 } as i64;
    let day_of_year = (153 * shifted + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4
        - year_of_era / 100
        + day_of_year;

    era * DAYS_PER_ERA + day_of_era - UNIX_SHIFT
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> Date {
    let days = days + UNIX_SHIFT;
    let era = if days >= 0 { days } else { days - 146_096 } / DAYS_PER_ERA;
    let day_of_era = days - era * DAYS_PER_ERA;
    let year_of_era = (day_of_era - day_of_era / 1460
        + day_of_era / 36524
        - day_of_era / 146_096)
        / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era
        - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted + 2) / 5 + 1) as u32;
    let month =
        if shifted < 10 { shifted + 3 } else { shifted - 9 } as u32;

    Date { year: if month <= 2 { year + 1 } else { year }, month, day }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_is_where_it_should_be() {
        let epoch = Date { year: 1970, month: 1, day: 1 };
        assert_eq!(epoch.midnight(), 0);
        assert_eq!(Date::of(0), epoch);
        assert_eq!(format(0), "1970-01-01");
    }

    /// Every date in forty years, both ways. This is the whole point of
    /// writing the algorithm out rather than approximating it.
    #[test]
    fn every_date_round_trips() {
        let mut expected = Date { year: 1990, month: 1, day: 1 };
        let mut seconds = expected.midnight();

        while expected.year < 2030 {
            assert_eq!(Date::of(seconds), expected, "at {seconds}");
            assert_eq!(expected.midnight(), seconds, "at {expected}");
            // Any moment inside the day reads back as the same day.
            assert_eq!(Date::of(seconds + 86_399), expected);

            seconds += SECONDS_PER_DAY;
            expected.day += 1;
            if expected.day > days_in_month(expected.year, expected.month)
            {
                expected.day = 1;
                expected.month += 1;
                if expected.month > 12 {
                    expected.month = 1;
                    expected.year += 1;
                }
            }
        }
    }

    /// The four cases a hand-rolled leap rule gets wrong.
    #[test]
    fn the_leap_year_rules_are_all_three_of_them() {
        // 2000 is a leap year (divisible by 400).
        assert!(Date::parse("2000-02-29").is_ok());
        // 1900 is not (divisible by 100 but not 400).
        assert!(Date::parse("1900-02-29").is_err());
        // 2024 is (divisible by 4).
        assert!(Date::parse("2024-02-29").is_ok());
        // 2023 is not.
        assert!(Date::parse("2023-02-29").is_err());

        // And the arithmetic agrees with the parser.
        let leap_day = Date { year: 2024, month: 2, day: 29 };
        assert_eq!(Date::of(leap_day.midnight()), leap_day);
        assert_eq!(
            Date::of(leap_day.midnight() + SECONDS_PER_DAY),
            Date { year: 2024, month: 3, day: 1 }
        );
    }

    #[test]
    fn a_date_is_read_strictly_or_not_at_all() {
        for good in ["2024-01-31", "1970-01-01", "2026-12-31"] {
            Date::parse(good).unwrap_or_else(|e| panic!("{good}: {e}"));
        }

        for bad in [
            "2024-1-5",
            "2024/01/05",
            "24-01-05",
            "2024-01-32",
            "2024-13-01",
            "2024-00-01",
            "2024-01-00",
            "yesterday",
            "",
            "2024-01-05T00:00:00Z",
            "1969-12-31",
        ] {
            let why =
                Date::parse(bad).expect_err("{bad:?} was read as a date");
            assert!(why.contains("YYYY-MM-DD"), "{why}");
        }
    }

    #[test]
    fn a_day_starts_at_midnight_utc() {
        let noon =
            Date { year: 2024, month: 6, day: 15 }.midnight() + 43_200;
        assert_eq!(format(noon), "2024-06-15");
        assert_eq!(
            start_of_day(noon),
            Date { year: 2024, month: 6, day: 15 }.midnight()
        );
        assert_eq!(start_of_day(0), 0);
    }
}
