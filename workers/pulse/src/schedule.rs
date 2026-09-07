use chrono::{DateTime, Datelike, Timelike, Utc};
use std::str::FromStr;

/// Membership of a UTC due slot. The engine, not this worker, schedules future runs.
#[derive(Debug)]
pub(super) struct Schedule {
    fields: [u64; 6],
}

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];
const WEEKDAYS: [&str; 7] = [
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
];
const LIMITS: [(u32, u32); 6] = [(0, 59), (0, 59), (0, 23), (1, 31), (1, 12), (1, 7)];

fn number(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn ordinal(text: &str, field: usize) -> Option<u32> {
    let names: &[&str] = match field {
        4 => &MONTHS,
        5 => &WEEKDAYS,
        _ => &[],
    };
    let value = number(text).or_else(|| {
        names
            .iter()
            .position(|name| {
                text.eq_ignore_ascii_case(name) || text.eq_ignore_ascii_case(&name[..3])
            })
            .map(|index| index as u32 + 1)
    })?;
    let (minimum, maximum) = LIMITS[field];
    (minimum..=maximum).contains(&value).then_some(value)
}

fn parse_field(text: &str, field: usize) -> Result<u64, &'static str> {
    let (minimum, maximum) = LIMITS[field];
    let mut mask = 0;
    for item in text.split(',') {
        let (base, step) = match item.split_once('/') {
            Some((base, step)) => (base, Some(number(step).ok_or("invalid cron step")?)),
            None => (item, None),
        };
        let (start, end) = if base == "*" || (base == "?" && matches!(field, 3 | 5)) {
            (minimum, maximum)
        } else if let Some((start, end)) = base.split_once('-') {
            if number(start).is_some() != number(end).is_some() {
                return Err("cron range endpoints must use the same notation");
            }
            (
                ordinal(start, field).ok_or("invalid cron range")?,
                ordinal(end, field).ok_or("invalid cron range")?,
            )
        } else {
            let start = ordinal(base, field).ok_or("invalid cron value")?;
            if step.is_some() && number(base).is_none() {
                return Err("a named cron point cannot have a step");
            }
            (start, if step.is_some() { maximum } else { start })
        };
        let step = step.unwrap_or(1);
        if start > end || step == 0 {
            return Err("invalid cron range or step");
        }
        for value in (start..=end).step_by(step as usize) {
            mask |= 1_u64 << value;
        }
    }
    Ok(mask)
}

impl FromStr for Schedule {
    type Err = &'static str;

    fn from_str(expression: &str) -> Result<Self, Self::Err> {
        let mut fields = [0; 6];
        let mut tokens = expression.split_whitespace();
        for (index, field) in fields.iter_mut().enumerate() {
            *field = parse_field(
                tokens.next().ok_or("cron requires six normalized fields")?,
                index,
            )?;
        }
        if tokens.next().is_some() {
            return Err("cron requires six normalized fields");
        }
        Ok(Self { fields })
    }
}

impl Schedule {
    pub(super) fn includes(&self, time: DateTime<Utc>) -> bool {
        let values = [
            time.second(),
            time.minute(),
            time.hour(),
            time.day(),
            time.month(),
            time.weekday().number_from_sunday(),
        ];
        (1970..=2100).contains(&time.year())
            && self
                .fields
                .iter()
                .zip(values)
                .all(|(mask, value)| mask & (1_u64 << value) != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn fields_accept_lists_ranges_steps_names_and_day_wildcards() {
        let schedule = Schedule::from_str("0,30 10-20/5 8/2 ? Jan-March/2 MON-FRI").unwrap();
        assert!(schedule.includes(Utc.with_ymd_and_hms(2026, 3, 2, 10, 15, 30).unwrap()));
        assert!(!schedule.includes(Utc.with_ymd_and_hms(2026, 3, 1, 10, 15, 30).unwrap()));
        assert!(!schedule.includes(Utc.with_ymd_and_hms(2026, 2, 2, 10, 15, 30).unwrap()));
    }

    #[test]
    fn preserves_sunday_one_and_calendar_weekday_intersection() {
        let schedule = Schedule::from_str("0 0 12 6 * 1").unwrap();
        assert!(schedule.includes(Utc.with_ymd_and_hms(2026, 9, 6, 12, 0, 0).unwrap()));
        assert!(!schedule.includes(Utc.with_ymd_and_hms(2026, 9, 13, 12, 0, 0).unwrap()));
        assert!(!schedule.includes(Utc.with_ymd_and_hms(2026, 10, 6, 12, 0, 0).unwrap()));
        assert!(Schedule::from_str("0 0 0 * * 0").is_err());
    }

    #[test]
    fn validates_field_bounds_and_does_not_add_extended_cron_syntax() {
        for expression in [
            "60 * * * * *",
            "* 60 * * * *",
            "* * 24 * * *",
            "* * * 0 * *",
            "* * * * 13 *",
            "* * * * * 8",
            "*/0 * * * * *",
            "0,,1 * * * * *",
            "0 0 0 L * *",
            "0 0 0 * * MON#2",
            "0 0 0 * * MON/2",
            "0 0 0 * * MON-6",
            "0 0 0 * * ? *",
            "0 0 0 * *",
            "0 0 0 * * * trailing",
        ] {
            assert!(Schedule::from_str(expression).is_err(), "{expression}");
        }
    }

    #[test]
    fn matches_legacy_cron_corpus() {
        let cases: Vec<(String, Option<[String; 6]>)> =
            serde_json::from_str(include_str!("../tests/cron-corpus.json")).unwrap();
        assert_eq!(cases.len(), 176);
        for (expression, expected) in cases {
            let actual = Schedule::from_str(&expression)
                .ok()
                .map(|schedule| schedule.fields.map(|mask| format!("{mask:016x}")));
            assert_eq!(actual, expected, "{expression}");
        }
    }
}
