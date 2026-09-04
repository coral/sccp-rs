//! Typed, recurring weekly do-not-disturb schedules.

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

pub const MAX_DND_SCHEDULES: usize = 32;
pub const MAX_DND_SCHEDULE_BYTES: usize = 128;

const END_OF_DAY_MINUTE: u16 = 24 * 60;
const MINUTES_PER_DAY: usize = 24 * 60;
const MINUTES_PER_WEEK: usize = 7 * MINUTES_PER_DAY;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Weekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl Weekday {
    const ALL: [Self; 7] = [
        Self::Monday,
        Self::Tuesday,
        Self::Wednesday,
        Self::Thursday,
        Self::Friday,
        Self::Saturday,
        Self::Sunday,
    ];

    const fn index(self) -> u8 {
        match self {
            Self::Monday => 0,
            Self::Tuesday => 1,
            Self::Wednesday => 2,
            Self::Thursday => 3,
            Self::Friday => 4,
            Self::Saturday => 5,
            Self::Sunday => 6,
        }
    }

    const fn canonical(self) -> &'static str {
        match self {
            Self::Monday => "mon",
            Self::Tuesday => "tue",
            Self::Wednesday => "wed",
            Self::Thursday => "thu",
            Self::Friday => "fri",
            Self::Saturday => "sat",
            Self::Sunday => "sun",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Monday => Self::Tuesday,
            Self::Tuesday => Self::Wednesday,
            Self::Wednesday => Self::Thursday,
            Self::Thursday => Self::Friday,
            Self::Friday => Self::Saturday,
            Self::Saturday => Self::Sunday,
            Self::Sunday => Self::Monday,
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        Self::ALL
            .into_iter()
            .find(|weekday| raw.eq_ignore_ascii_case(weekday.canonical()))
    }
}

impl fmt::Display for Weekday {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.canonical())
    }
}

/// A non-empty set of weekdays, represented in Monday-through-Sunday order.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WeekdaySet(u8);

impl WeekdaySet {
    const ALL_BITS: u8 = 0x7f;

    const fn all() -> Self {
        Self(Self::ALL_BITS)
    }

    const fn contains(self, day: Weekday) -> bool {
        self.0 & (1 << day.index()) != 0
    }

    fn iter(self) -> impl Iterator<Item = Weekday> {
        Weekday::ALL
            .into_iter()
            .filter(move |day| self.contains(*day))
    }

    const fn shifted_to_next_day(self) -> Self {
        Self(((self.0 << 1) | (self.0 >> 6)) & Self::ALL_BITS)
    }

    fn insert(&mut self, day: Weekday) {
        self.0 |= 1 << day.index();
    }

    fn parse(raw: &str) -> Result<Self, DndScheduleParseError> {
        let raw = raw.trim();
        if raw == "*" {
            return Ok(Self::all());
        }
        if raw.is_empty() || raw.contains('*') {
            return Err(DndScheduleParseError::InvalidDays(raw.into()));
        }

        let mut result = Self(0);
        for part in raw.split('&') {
            let part = part.trim();
            if part.is_empty() {
                return Err(DndScheduleParseError::InvalidDays(raw.into()));
            }
            let mut range = part.split('-');
            let Some(first) = range.next().and_then(Weekday::parse) else {
                return Err(DndScheduleParseError::InvalidDays(raw.into()));
            };
            match range.next() {
                None => result.insert(first),
                Some(last_raw) => {
                    let Some(last) = Weekday::parse(last_raw) else {
                        return Err(DndScheduleParseError::InvalidDays(raw.into()));
                    };
                    if range.next().is_some() {
                        return Err(DndScheduleParseError::InvalidDays(raw.into()));
                    }
                    let mut day = first;
                    loop {
                        result.insert(day);
                        if day == last {
                            break;
                        }
                        day = day.next();
                    }
                }
            }
        }
        if result.0 == 0 {
            Err(DndScheduleParseError::InvalidDays(raw.into()))
        } else {
            Ok(result)
        }
    }
}

impl fmt::Display for WeekdaySet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == Self::ALL_BITS {
            return formatter.write_str("*");
        }

        let mut first_output = true;
        let mut days = self.iter().peekable();
        while let Some(start) = days.next() {
            let mut end = start;
            while days
                .peek()
                .is_some_and(|next| next.index() == end.index() + 1)
            {
                if let Some(next) = days.next() {
                    end = next;
                }
            }
            if !first_output {
                formatter.write_str("&")?;
            }
            first_output = false;
            formatter.write_str(start.canonical())?;
            if start != end {
                formatter.write_str("-")?;
                formatter.write_str(end.canonical())?;
            }
        }
        Ok(())
    }
}

/// One same-day half-open range within a weekly schedule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DndScheduleSegment {
    start_minute: u16,
    end_minute_exclusive: u16,
    weekdays: WeekdaySet,
}

impl DndScheduleSegment {
    pub const fn start_minute(self) -> u16 {
        self.start_minute
    }

    pub const fn end_minute_exclusive(self) -> u16 {
        self.end_minute_exclusive
    }

    pub const fn weekdays(self) -> WeekdaySet {
        self.weekdays
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DndScheduleMode {
    Silent,
    Reject,
}

impl fmt::Display for DndScheduleMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Silent => "silent",
            Self::Reject => "reject",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DndScheduleEnd {
    Minute(u16),
    EndOfDay,
}

impl DndScheduleEnd {
    const fn minute(self) -> u16 {
        match self {
            Self::Minute(minute) => minute,
            Self::EndOfDay => 0,
        }
    }

    const fn effective_minute(self) -> u16 {
        match self {
            Self::Minute(minute) => minute,
            Self::EndOfDay => END_OF_DAY_MINUTE,
        }
    }
}

/// A weekly DND window. Its start is inclusive and its end is exclusive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DndSchedule {
    start_minute: u16,
    end: DndScheduleEnd,
    weekdays: WeekdaySet,
    mode: DndScheduleMode,
}

impl DndSchedule {
    pub fn parse(raw: &str) -> Result<Self, DndScheduleParseError> {
        raw.parse()
    }

    pub const fn mode(&self) -> DndScheduleMode {
        self.mode
    }

    pub fn timing_segments(&self) -> Vec<DndScheduleSegment> {
        let effective_end = self.end.effective_minute();
        if self.start_minute < effective_end {
            return vec![DndScheduleSegment {
                start_minute: self.start_minute,
                end_minute_exclusive: effective_end,
                weekdays: self.weekdays,
            }];
        }

        let mut segments = vec![DndScheduleSegment {
            start_minute: self.start_minute,
            end_minute_exclusive: END_OF_DAY_MINUTE,
            weekdays: self.weekdays,
        }];
        let end_minute = self.end.minute();
        if end_minute != 0 {
            segments.push(DndScheduleSegment {
                start_minute: 0,
                end_minute_exclusive: end_minute,
                weekdays: self.weekdays.shifted_to_next_day(),
            });
        }
        segments
    }

    fn duration_minutes(&self) -> usize {
        let start = usize::from(self.start_minute);
        let end = usize::from(self.end.effective_minute());
        if start < end {
            end - start
        } else {
            MINUTES_PER_DAY - start + end
        }
    }
}

impl FromStr for DndSchedule {
    type Err = DndScheduleParseError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.len() > MAX_DND_SCHEDULE_BYTES {
            return Err(DndScheduleParseError::TooLong {
                bytes: raw.len(),
                maximum: MAX_DND_SCHEDULE_BYTES,
            });
        }
        let mut fields = raw.split(',');
        let (time_range, days, mode) = match (
            fields.next().map(str::trim),
            fields.next().map(str::trim),
            fields.next().map(str::trim),
            fields.next(),
        ) {
            (Some(time_range), Some(days), Some(mode), None) if !time_range.is_empty() => {
                (time_range, days, mode)
            }
            _ => return Err(DndScheduleParseError::InvalidFormat),
        };

        let mut times = time_range.split('-');
        let (start_raw, end_raw) = match (
            times.next().map(str::trim),
            times.next().map(str::trim),
            times.next(),
        ) {
            (Some(start), Some(end), None) if !start.is_empty() && !end.is_empty() => (start, end),
            _ => return Err(DndScheduleParseError::InvalidTimeRange(time_range.into())),
        };
        let start_minute = parse_clock(start_raw, false)?;
        let parsed_end = parse_clock(end_raw, true)?;
        let end = match parsed_end {
            END_OF_DAY_MINUTE => DndScheduleEnd::EndOfDay,
            minute => DndScheduleEnd::Minute(minute),
        };
        if start_minute == end.minute() && !matches!(end, DndScheduleEnd::EndOfDay) {
            return Err(DndScheduleParseError::EqualTimes);
        }

        let weekdays = WeekdaySet::parse(days)?;
        let mode = if mode.eq_ignore_ascii_case("silent") {
            DndScheduleMode::Silent
        } else if mode.eq_ignore_ascii_case("reject") {
            DndScheduleMode::Reject
        } else {
            return Err(DndScheduleParseError::InvalidMode(mode.into()));
        };
        Ok(Self {
            start_minute,
            end,
            weekdays,
            mode,
        })
    }
}

impl fmt::Display for DndSchedule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let end = match self.end {
            DndScheduleEnd::Minute(minute) => format_minute(usize::from(minute)),
            DndScheduleEnd::EndOfDay => "24:00".into(),
        };
        write!(
            formatter,
            "{}-{end}, {}, {}",
            format_minute(usize::from(self.start_minute)),
            self.weekdays,
            self.mode,
        )
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DndScheduleParseError {
    #[error("DND schedule is {bytes} bytes; maximum is {maximum}")]
    TooLong { bytes: usize, maximum: usize },
    #[error("expected HH:MM-HH:MM, <days>, <silent|reject>")]
    InvalidFormat,
    #[error("invalid time range {0:?}; expected HH:MM-HH:MM")]
    InvalidTimeRange(String),
    #[error("invalid start time {0:?}; expected 00:00 through 23:59")]
    InvalidStartTime(String),
    #[error("invalid end time {0:?}; expected 00:00 through 24:00")]
    InvalidEndTime(String),
    #[error("schedule start and end must not be equal")]
    EqualTimes,
    #[error("invalid weekdays {0:?}; expected *, mon..sun, ranges, or & unions")]
    InvalidDays(String),
    #[error("invalid DND mode {0:?}; expected silent or reject")]
    InvalidMode(String),
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DndScheduleValidationError {
    #[error("configured {count} DND schedules; maximum is {maximum}")]
    TooMany { count: usize, maximum: usize },
    #[error("DND schedule {second} overlaps schedule {first} at {weekday} {time}")]
    Overlap {
        first: usize,
        second: usize,
        weekday: Weekday,
        time: String,
    },
}

pub fn validate_dnd_schedules(schedules: &[DndSchedule]) -> Result<(), DndScheduleValidationError> {
    if schedules.len() > MAX_DND_SCHEDULES {
        return Err(DndScheduleValidationError::TooMany {
            count: schedules.len(),
            maximum: MAX_DND_SCHEDULES,
        });
    }

    let mut occupied = vec![None; MINUTES_PER_WEEK];
    for (index, schedule) in schedules.iter().enumerate() {
        for weekday in schedule.weekdays.iter() {
            let start =
                usize::from(weekday.index()) * MINUTES_PER_DAY + usize::from(schedule.start_minute);
            for offset in 0..schedule.duration_minutes() {
                let minute = (start + offset) % MINUTES_PER_WEEK;
                if let Some(previous) = occupied[minute] {
                    return Err(DndScheduleValidationError::Overlap {
                        first: previous + 1,
                        second: index + 1,
                        weekday: Weekday::ALL[minute / MINUTES_PER_DAY],
                        time: format_minute(minute % MINUTES_PER_DAY),
                    });
                }
                occupied[minute] = Some(index);
            }
        }
    }
    Ok(())
}

fn parse_clock(raw: &str, end: bool) -> Result<u16, DndScheduleParseError> {
    let bytes = raw.as_bytes();
    let invalid = || {
        if end {
            DndScheduleParseError::InvalidEndTime(raw.into())
        } else {
            DndScheduleParseError::InvalidStartTime(raw.into())
        }
    };
    if bytes.len() != 5
        || bytes[2] != b':'
        || !bytes[0].is_ascii_digit()
        || !bytes[1].is_ascii_digit()
        || !bytes[3].is_ascii_digit()
        || !bytes[4].is_ascii_digit()
    {
        return Err(invalid());
    }
    let hour = u16::from(bytes[0] - b'0') * 10 + u16::from(bytes[1] - b'0');
    let minute = u16::from(bytes[3] - b'0') * 10 + u16::from(bytes[4] - b'0');
    if minute > 59 || hour > 24 || hour == 24 && (!end || minute != 0) {
        return Err(invalid());
    }
    Ok(hour * 60 + minute)
}

fn format_minute(minute: usize) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalizes_weekly_schedules() {
        let schedule = DndSchedule::parse("22:00-07:00, FRI-MON & wed, ReJeCt").unwrap();
        assert_eq!(schedule.start_minute, 22 * 60);
        assert_eq!(schedule.end, DndScheduleEnd::Minute(7 * 60));
        assert_eq!(schedule.mode(), DndScheduleMode::Reject);
        assert_eq!(schedule.to_string(), "22:00-07:00, mon&wed&fri-sun, reject");
        assert_eq!(DndSchedule::parse(&schedule.to_string()).unwrap(), schedule);

        let all_day = DndSchedule::parse("00:00-24:00, *, silent").unwrap();
        assert_eq!(all_day.end, DndScheduleEnd::EndOfDay);
        assert_eq!(all_day.to_string(), "00:00-24:00, *, silent");
    }

    #[test]
    fn rejects_malformed_times_days_modes_and_equal_endpoints() {
        for raw in [
            "22:00-07:00, *",
            "2:00-07:00, *, silent",
            "22:60-07:00, *, silent",
            "24:00-07:00, *, silent",
            "22:00-24:01, *, silent",
            "22:00-22:00, *, silent",
            "22:00-07:00, weekday, silent",
            "22:00-07:00, mon&&tue, silent",
            "22:00-07:00, *, off",
        ] {
            assert!(DndSchedule::parse(raw).is_err(), "accepted {raw:?}");
        }
    }

    #[test]
    fn schedule_text_limit_accepts_the_boundary_and_rejects_one_more_byte() {
        let rule = "22:00-07:00, *, reject";
        let boundary = format!("{}{rule}", " ".repeat(MAX_DND_SCHEDULE_BYTES - rule.len()));
        assert!(DndSchedule::parse(&boundary).is_ok());

        let overflow = format!(" {boundary}");
        assert!(matches!(
            DndSchedule::parse(&overflow),
            Err(DndScheduleParseError::TooLong {
                bytes,
                maximum: MAX_DND_SCHEDULE_BYTES,
            }) if bytes == MAX_DND_SCHEDULE_BYTES + 1
        ));
    }

    #[test]
    fn splits_overnight_timing_at_midnight_and_shifts_days() {
        let schedule = DndSchedule::parse("22:00-07:00, fri-sun, reject").unwrap();
        assert_eq!(
            schedule.timing_segments(),
            [
                DndScheduleSegment {
                    start_minute: 22 * 60,
                    end_minute_exclusive: 24 * 60,
                    weekdays: WeekdaySet::parse("fri-sun").unwrap(),
                },
                DndScheduleSegment {
                    start_minute: 0,
                    end_minute_exclusive: 7 * 60,
                    weekdays: WeekdaySet::parse("sat-mon").unwrap(),
                },
            ]
        );

        let until_midnight = DndSchedule::parse("22:00-00:00, mon, silent").unwrap();
        assert_eq!(
            until_midnight.timing_segments(),
            [DndScheduleSegment {
                start_minute: 22 * 60,
                end_minute_exclusive: 24 * 60,
                weekdays: WeekdaySet::parse("mon").unwrap(),
            }]
        );
    }

    #[test]
    fn validates_expanded_weekly_overlap_and_allows_adjacency() {
        let schedules = [
            DndSchedule::parse("22:00-07:00, mon, reject").unwrap(),
            DndSchedule::parse("07:00-08:00, tue, silent").unwrap(),
        ];
        validate_dnd_schedules(&schedules).unwrap();

        let overlapping = [
            schedules[0].clone(),
            DndSchedule::parse("06:59-08:00, tue, silent").unwrap(),
        ];
        assert!(matches!(
            validate_dnd_schedules(&overlapping),
            Err(DndScheduleValidationError::Overlap {
                first: 1,
                second: 2,
                weekday: Weekday::Tuesday,
                ref time,
            }) if time == "06:59"
        ));

        let duplicate = [schedules[0].clone(), schedules[0].clone()];
        assert!(matches!(
            validate_dnd_schedules(&duplicate),
            Err(DndScheduleValidationError::Overlap { .. })
        ));
    }

    #[test]
    fn enforces_the_per_device_limit() {
        let schedule = DndSchedule::parse("00:00-00:01, mon, silent").unwrap();
        let schedules = vec![schedule; MAX_DND_SCHEDULES + 1];
        assert!(matches!(
            validate_dnd_schedules(&schedules),
            Err(DndScheduleValidationError::TooMany { .. })
        ));
    }
}
