use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::path::PathBuf;

pub mod server;
pub mod store;

pub type BoxError = Box<dyn Error + Send + Sync>;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CalendarFile {
    pub id: String,
    pub title: String,
    pub year: i32,
    pub region: String,
    pub exam_type: String,
    pub timezone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub default_alarm_minutes: Vec<i64>,
    #[serde(default)]
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Source {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Event {
    pub id: String,
    pub title: String,
    pub start: String,
    pub end: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "is_false")]
    pub all_day: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<EventStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alarm_minutes: Option<Vec<i64>>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    Confirmed,
    Tentative,
    Cancelled,
}

impl EventStatus {
    pub fn as_ics(&self) -> &'static str {
        match self {
            Self::Confirmed => "CONFIRMED",
            Self::Tentative => "TENTATIVE",
            Self::Cancelled => "CANCELLED",
        }
    }
}

impl EventStatus {
    pub fn as_json_value(&self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Tentative => "tentative",
            Self::Cancelled => "cancelled",
        }
    }
}

impl TryFrom<&str> for EventStatus {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "confirmed" => Ok(Self::Confirmed),
            "tentative" => Ok(Self::Tentative),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(format!("invalid event status '{value}'")),
        }
    }
}

#[derive(Debug)]
pub struct GenerationOptions {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
}

pub fn generate_all(options: &GenerationOptions) -> Result<Vec<PathBuf>, BoxError> {
    fs::create_dir_all(&options.output_dir)?;

    let mut json_files = Vec::new();
    for entry in fs::read_dir(&options.input_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            json_files.push(path);
        }
    }
    json_files.sort();

    let mut written = Vec::new();
    for input_path in json_files {
        let text = fs::read_to_string(&input_path)?;
        let calendar: CalendarFile = serde_json::from_str(&text)
            .map_err(|err| format!("{}: invalid JSON: {err}", input_path.display()))?;
        validate_calendar(&calendar).map_err(|err| format!("{}: {err}", input_path.display()))?;

        let ics = render_calendar(&calendar)?;
        let output_path = options.output_dir.join(format!("{}.ics", calendar.id));
        fs::write(&output_path, ics)?;
        written.push(output_path);
    }

    Ok(written)
}

pub fn render_calendar(calendar: &CalendarFile) -> Result<String, BoxError> {
    validate_calendar(calendar)?;

    let mut lines = Vec::new();
    lines.push("BEGIN:VCALENDAR".to_string());
    lines.push("VERSION:2.0".to_string());
    lines.push("PRODID:-//exam-calendar//exam-calendar 0.1//EN".to_string());
    lines.push("CALSCALE:GREGORIAN".to_string());
    lines.push("METHOD:PUBLISH".to_string());
    property(&mut lines, "X-WR-CALNAME", &calendar.title);
    property(&mut lines, "X-WR-TIMEZONE", &calendar.timezone);
    property(&mut lines, "X-EXAM-REGION", &calendar.region);
    property(&mut lines, "X-EXAM-TYPE", &calendar.exam_type);

    if let Some(source) = &calendar.source {
        property(&mut lines, "X-EXAM-SOURCE", &source.name);
        property(&mut lines, "X-EXAM-SOURCE-URL", &source.url);
    }

    let dtstamp = calendar_dtstamp(calendar)?;
    for event in &calendar.events {
        render_event(calendar, event, &dtstamp, &mut lines)?;
    }

    lines.push("END:VCALENDAR".to_string());

    let mut out = String::new();
    for line in lines {
        out.push_str(&fold_line(&line));
        out.push_str("\r\n");
    }
    Ok(out)
}

fn render_event(
    calendar: &CalendarFile,
    event: &Event,
    dtstamp: &str,
    lines: &mut Vec<String>,
) -> Result<(), BoxError> {
    lines.push("BEGIN:VEVENT".to_string());
    property_raw(
        lines,
        "UID",
        &format!(
            "{}.{}.{}@exam-calendar",
            calendar.id, calendar.year, event.id
        ),
    );
    property_raw(lines, "DTSTAMP", dtstamp);
    property(lines, "SUMMARY", &event.title);

    if event.all_day {
        let start = parse_date(&event.start)?;
        let end = parse_date(&event.end)?;
        property_raw(
            lines,
            "DTSTART;VALUE=DATE",
            &start.format("%Y%m%d").to_string(),
        );
        property_raw(lines, "DTEND;VALUE=DATE", &end.format("%Y%m%d").to_string());
    } else {
        let start = parse_datetime(&event.start)?;
        let end = parse_datetime(&event.end)?;
        property_raw(
            lines,
            &format!("DTSTART;TZID={}", calendar.timezone),
            &start.format("%Y%m%dT%H%M%S").to_string(),
        );
        property_raw(
            lines,
            &format!("DTEND;TZID={}", calendar.timezone),
            &end.format("%Y%m%dT%H%M%S").to_string(),
        );
    }

    if let Some(description) = &event.description {
        property(lines, "DESCRIPTION", description);
    }
    if let Some(location) = &event.location {
        property(lines, "LOCATION", location);
    }
    if let Some(url) = &event.url {
        property_raw(lines, "URL", url);
    }
    if let Some(source) = &event.source {
        property(lines, "X-EXAM-SOURCE", &source.name);
        property_raw(lines, "X-EXAM-SOURCE-URL", &source.url);
    }
    if let Some(status) = &event.status {
        property_raw(lines, "STATUS", status.as_ics());
    }

    let alarms = event
        .alarm_minutes
        .as_ref()
        .unwrap_or(&calendar.default_alarm_minutes);
    for minutes in alarms {
        lines.push("BEGIN:VALARM".to_string());
        property_raw(lines, "ACTION", "DISPLAY");
        property(lines, "DESCRIPTION", &event.title);
        property_raw(lines, "TRIGGER", &format!("-PT{}M", minutes));
        lines.push("END:VALARM".to_string());
    }

    lines.push("END:VEVENT".to_string());
    Ok(())
}

pub fn validate_calendar(calendar: &CalendarFile) -> Result<(), BoxError> {
    require_token("calendar id", &calendar.id)?;
    if calendar.title.trim().is_empty() {
        return Err("calendar title is required".into());
    }
    if !(1900..=9999).contains(&calendar.year) {
        return Err("calendar year must be between 1900 and 9999".into());
    }
    if calendar.region.trim().is_empty() {
        return Err("calendar region is required".into());
    }
    if calendar.exam_type.trim().is_empty() {
        return Err("calendar exam_type is required".into());
    }
    if calendar.timezone.trim().is_empty() {
        return Err("calendar timezone is required".into());
    }
    if let Some(updated_at) = &calendar.updated_at {
        parse_rfc3339(updated_at)?;
    }

    for minutes in &calendar.default_alarm_minutes {
        validate_alarm(*minutes)?;
    }

    for event in &calendar.events {
        validate_event(calendar, event)?;
    }

    Ok(())
}

fn validate_event(calendar: &CalendarFile, event: &Event) -> Result<(), BoxError> {
    require_token("event id", &event.id)?;
    if event.title.trim().is_empty() {
        return Err(format!("event {} title is required", event.id).into());
    }

    if event.all_day {
        let start = parse_date(&event.start)?;
        let end = parse_date(&event.end)?;
        if end <= start {
            return Err(format!("event {} end must be after start", event.id).into());
        }
    } else {
        let start = parse_datetime(&event.start)?;
        let end = parse_datetime(&event.end)?;
        if end <= start {
            return Err(format!("event {} end must be after start", event.id).into());
        }
    }

    let alarms = event
        .alarm_minutes
        .as_ref()
        .unwrap_or(&calendar.default_alarm_minutes);
    for minutes in alarms {
        validate_alarm(*minutes)?;
    }

    Ok(())
}

fn require_token(label: &str, value: &str) -> Result<(), BoxError> {
    if value.trim().is_empty() {
        return Err(format!("{label} is required").into());
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(
            format!("{label} can only contain ASCII letters, numbers, '-', '_', '.'").into(),
        );
    }
    Ok(())
}

fn validate_alarm(minutes: i64) -> Result<(), BoxError> {
    if minutes <= 0 {
        return Err("alarm minutes must be positive".into());
    }
    Ok(())
}

fn parse_date(value: &str) -> Result<NaiveDate, BoxError> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| format!("invalid date '{value}', expected YYYY-MM-DD").into())
}

fn parse_datetime(value: &str) -> Result<NaiveDateTime, BoxError> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S")
        .map_err(|_| format!("invalid datetime '{value}', expected YYYY-MM-DDTHH:MM:SS").into())
}

fn parse_rfc3339(value: &str) -> Result<DateTime<Utc>, BoxError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| format!("invalid updated_at '{value}', expected RFC3339").into())
}

fn calendar_dtstamp(calendar: &CalendarFile) -> Result<String, BoxError> {
    let Some(updated_at) = &calendar.updated_at else {
        return Ok("19700101T000000Z".to_string());
    };

    Ok(parse_rfc3339(updated_at)?
        .format("%Y%m%dT%H%M%SZ")
        .to_string())
}

fn property(lines: &mut Vec<String>, name: &str, value: &str) {
    property_raw(lines, name, &escape_text(value));
}

fn property_raw(lines: &mut Vec<String>, name: &str, value: &str) {
    lines.push(format!("{name}:{value}"));
}

fn escape_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace("\r\n", "\\n")
        .replace('\n', "\\n")
        .replace('\r', "\\n")
}

fn fold_line(line: &str) -> String {
    let mut output = String::new();
    let mut rest = line;
    let mut limit = 75;

    while rest.as_bytes().len() > limit {
        let mut cut = limit;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        output.push_str(&rest[..cut]);
        output.push_str("\r\n ");
        rest = &rest[cut..];
        limit = 74;
    }

    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_calendar() -> CalendarFile {
        CalendarFile {
            id: "shanghai-gaokao".to_string(),
            title: "上海高考日程 2026".to_string(),
            year: 2026,
            region: "CN-SH".to_string(),
            exam_type: "gaokao".to_string(),
            timezone: "Asia/Shanghai".to_string(),
            updated_at: Some("2026-03-16T00:00:00Z".to_string()),
            source: None,
            default_alarm_minutes: vec![1440],
            events: vec![Event {
                id: "main".to_string(),
                title: "普通高校招生全国统一考试".to_string(),
                start: "2026-06-07".to_string(),
                end: "2026-06-10".to_string(),
                all_day: true,
                description: Some("以上海市教育考试院最终发布为准。".to_string()),
                location: None,
                url: None,
                source: None,
                status: Some(EventStatus::Confirmed),
                alarm_minutes: Some(vec![1440, 60]),
            }],
        }
    }

    #[test]
    fn renders_all_day_event_with_exclusive_end_date() {
        let ics = render_calendar(&sample_calendar()).unwrap();

        assert!(ics.contains("BEGIN:VCALENDAR\r\n"));
        assert!(ics.contains("UID:shanghai-gaokao.2026.main@exam-calendar\r\n"));
        assert!(ics.contains("DTSTART;VALUE=DATE:20260607\r\n"));
        assert!(ics.contains("DTEND;VALUE=DATE:20260610\r\n"));
    }

    #[test]
    fn event_alarm_overrides_calendar_default() {
        let ics = render_calendar(&sample_calendar()).unwrap();

        assert!(ics.contains("TRIGGER:-PT1440M\r\n"));
        assert!(ics.contains("TRIGGER:-PT60M\r\n"));
    }

    #[test]
    fn rejects_non_exclusive_all_day_end() {
        let mut calendar = sample_calendar();
        calendar.events[0].end = "2026-06-07".to_string();

        let err = render_calendar(&calendar).unwrap_err().to_string();

        assert!(err.contains("end must be after start"));
    }

    #[test]
    fn escapes_ics_text() {
        assert_eq!(escape_text("a,b;c\\d\nnext"), "a\\,b\\;c\\\\d\\nnext");
    }
}
