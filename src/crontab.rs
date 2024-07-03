use std::num::ParseIntError;
use chrono::{Datelike, Timelike};
use regex::Regex;

#[derive(Debug)]
pub enum ParseError {
    InvalidFormat(String),
    ParseIntError(ParseIntError),
}

impl From<ParseIntError> for ParseError {
    fn from(err: ParseIntError) -> ParseError {
        ParseError::ParseIntError(err)
    }
}

fn validate_str(input:&str) -> bool {
    let single_number_regex = Regex::new(r"^([1-9]\d*(?:/[1-9]\d*)?)|0$").unwrap();
    let range_regex = Regex::new(r"^(([1-9]\d*|0)-(0|[1-9]\d*))(?:/[1-9]\d*)?$").unwrap();
    let wildcard_regex = Regex::new(r"^\*(?:/[1-9]\d*)?$$").unwrap();
    return single_number_regex.is_match(input) 
        || range_regex.is_match(input)
        || wildcard_regex.is_match(input)
}


fn parse_multiple(spec:&str, min: usize, max:usize) -> Result<Vec<usize>, ParseError> {
    let parse_result = spec.split(',').map(|s| parse_spec(s, min, max));
    let mut result_vec  = Vec::new();
    for i in parse_result {
        match i {
            Ok(matches) => {
                for j in matches {
                    result_vec.push(j);
                }
            },
            Err(e) => return Err(e),
        }
    }

    result_vec.sort();
    result_vec.dedup();
    return Ok(result_vec)
}
fn parse_spec(spec: &str, min: usize, max: usize) -> Result<Vec<usize>, ParseError> {
    if !validate_str(spec) {
        return Err(ParseError::InvalidFormat(spec.to_string()));
    }
    let (range_part, step) = if let Some(pos) = spec.find('/') {
        let step: usize = spec[pos + 1..].parse()?;
        (&spec[..pos], Some(step))
    } else {
        (spec, None)
    };

    let mut result = vec![];

    if range_part == "*" {
        for i in min..max {
            if step.map_or(true, |step| i % step == 0) {
                result.push(i);
            }
        }
    } else if let Some(pos) = range_part.find('-') {
        let start: usize = range_part[..pos].parse()?;
        let end: usize = range_part[pos + 1..].parse()?;

        if start >= max || start < min || end >= max || end < min{
            return Err(ParseError::InvalidFormat(range_part.to_string()));
        }

        for i in start..=end {
            if step.map_or(true, |step| i % step == 0) {
                result.push(i);
            }
        }
    } else {
        let value: usize = range_part.parse()?;
        if value >= max || value < min{
            return Err(ParseError::InvalidFormat(range_part.to_string()));
        }

        if step.map_or(true, |step| value % step == 0) {
            result.push(value);
        }
    }

    Ok(result)
}


#[derive(Debug, Clone)]
pub struct Crontab {
    minute: [bool; 60],
    hour: [bool; 24],
    day_of_month: [bool; 31],
    month: [bool; 12],
    day_of_week: [bool; 7],
}

impl Crontab {
    pub fn new(spec: &str) -> Result<Crontab, ParseError> {
        let parts: Vec<&str> = spec.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(ParseError::InvalidFormat(spec.to_string()));
        }

        let minute = parse_multiple(parts[0], 0, 60)?;
        let hour = parse_multiple(parts[1], 0, 24)?;
        let day_of_month = parse_multiple(parts[2], 1, 32)?;
        let month = parse_multiple(parts[3], 1,13)?;
        let day_of_week = parse_multiple(parts[4], 0,7)?;

        let mut minute_array = [false; 60];
        let mut hour_array = [false; 24];
        let mut day_of_month_array = [false; 31];
        let mut month_array = [false; 12];
        let mut day_of_week_array = [false; 7];

        for i in minute {
            minute_array[i] = true;
        }
        for i in hour {
            hour_array[i] = true;
        }
        for i in day_of_month {
            day_of_month_array[i - 1] = true;
        }
        for i in month {
            month_array[i-1] = true;
        }
        for i in day_of_week {
            day_of_week_array[i] = true;
        }

        Ok(Crontab {
            minute: minute_array,
            hour: hour_array,
            day_of_month: day_of_month_array,
            month: month_array,
            day_of_week: day_of_week_array,
        })
    }

    /// Check if the given time matches the crontab specification.
    /// minute: 0-59
    /// hour: 0-23
    /// day_of_month: 1-31 where 1 is 1st day of the month
    /// month: 1-12 where 1 is January, 12 is December
    /// day_of_week: 0-6 (0=Sunday, 1 is Monday, 2 is Tuesday, and so on)
    pub fn is_match(&self, minute: usize, hour: usize, day_of_month: usize, month: usize, day_of_week: usize) -> bool {
        if minute >= 60 || hour >= 24 || day_of_month < 1 || day_of_month >= 32 || month < 1 || month >= 13 || day_of_week >= 7 {
            return false;
        }
        self.minute[minute]
            && self.hour[hour]
            && self.day_of_month[day_of_month - 1]
            && self.month[month - 1]
            && self.day_of_week[day_of_week]
    }

    /// Check if the current time matches the crontab specification.
    pub fn is_match_now(&self) -> bool {
        let now = chrono::Local::now();
        self.is_match(
            now.minute() as usize,
            now.hour() as usize,
            now.day() as usize,
            now.month() as usize,
            now.weekday().num_days_from_sunday() as usize,
        )
    }
}