use crate::{
    BoxError, CalendarFile, Event, EventStatus, GenerationOptions, Source, validate_calendar,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

type StoreResult<T> = Result<T, BoxError>;

#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct CalendarSummary {
    pub id: String,
    pub title: String,
    pub year: i32,
    pub region: String,
    pub exam_type: String,
}

impl Store {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn init(&self) -> StoreResult<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = self.open()?;
        init_schema(&conn)?;
        Ok(())
    }

    pub fn import_json_dir(&self, input_dir: &Path) -> StoreResult<Vec<String>> {
        self.init()?;
        let mut imported = Vec::new();
        let mut files = Vec::new();
        for entry in fs::read_dir(input_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
                files.push(path);
            }
        }
        files.sort();

        for path in files {
            let text = fs::read_to_string(&path)?;
            let calendar: CalendarFile = serde_json::from_str(&text)
                .map_err(|err| format!("{}: invalid JSON: {err}", path.display()))?;
            self.save_calendar(&calendar)?;
            imported.push(calendar.id);
        }

        Ok(imported)
    }

    pub fn export_json_dir(&self, output_dir: &Path) -> StoreResult<Vec<PathBuf>> {
        self.init()?;
        fs::create_dir_all(output_dir)?;

        let mut written = Vec::new();
        for summary in self.list_calendars()? {
            let calendar = self.load_calendar(&summary.id)?;
            let text = serde_json::to_string_pretty(&calendar)?;
            let path = output_dir.join(format!("{}.json", calendar.id));
            fs::write(&path, format!("{text}\n"))?;
            written.push(path);
        }

        Ok(written)
    }

    pub fn export_json_and_generate(
        &self,
        data_dir: &Path,
        output_dir: &Path,
    ) -> StoreResult<Vec<PathBuf>> {
        self.export_json_dir(data_dir)?;
        crate::generate_all(&GenerationOptions {
            input_dir: data_dir.to_path_buf(),
            output_dir: output_dir.to_path_buf(),
        })
        .map_err(|err| err.to_string().into())
    }

    pub fn list_calendars(&self) -> StoreResult<Vec<CalendarSummary>> {
        self.init()?;
        let conn = self.open()?;
        let mut stmt = conn.prepare(
            "SELECT id, title, year, region, exam_type
             FROM calendars
             ORDER BY region, exam_type, year, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(CalendarSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                year: row.get(2)?,
                region: row.get(3)?,
                exam_type: row.get(4)?,
            })
        })?;

        let mut calendars = Vec::new();
        for row in rows {
            calendars.push(row?);
        }
        Ok(calendars)
    }

    pub fn load_calendar(&self, id: &str) -> StoreResult<CalendarFile> {
        self.init()?;
        let conn = self.open()?;
        let calendar = conn
            .query_row(
                "SELECT id, title, year, region, exam_type, timezone, updated_at,
                        source_name, source_url, default_alarm_minutes_json
                 FROM calendars
                 WHERE id = ?1",
                [id],
                |row| {
                    let source_name: Option<String> = row.get(7)?;
                    let source_url: Option<String> = row.get(8)?;
                    let alarms_json: String = row.get(9)?;
                    let default_alarm_minutes = parse_i64_vec(&alarms_json).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            9,
                            rusqlite::types::Type::Text,
                            err,
                        )
                    })?;
                    Ok(CalendarFile {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        year: row.get(2)?,
                        region: row.get(3)?,
                        exam_type: row.get(4)?,
                        timezone: row.get(5)?,
                        updated_at: row.get(6)?,
                        source: source_from_columns(source_name, source_url),
                        default_alarm_minutes,
                        events: Vec::new(),
                    })
                },
            )
            .optional()?
            .ok_or_else(|| format!("calendar '{id}' not found"))?;

        let mut calendar = calendar;
        calendar.events = load_events(&conn, id)?;
        validate_calendar(&calendar).map_err(|err| err.to_string())?;
        Ok(calendar)
    }

    pub fn save_calendar(&self, calendar: &CalendarFile) -> StoreResult<()> {
        validate_calendar(calendar).map_err(|err| err.to_string())?;
        self.init()?;
        let mut conn = self.open()?;
        let tx = conn.transaction()?;
        upsert_calendar(&tx, calendar)?;
        tx.execute("DELETE FROM events WHERE calendar_id = ?1", [&calendar.id])?;
        for event in &calendar.events {
            upsert_event(&tx, &calendar.id, event)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_events(&self, calendar_id: &str, events: &[Event]) -> StoreResult<usize> {
        self.init()?;
        let mut calendar = self.load_calendar(calendar_id)?;

        for event in events {
            if let Some(existing) = calendar
                .events
                .iter_mut()
                .find(|existing| existing.id == event.id)
            {
                *existing = event.clone();
            } else {
                calendar.events.push(event.clone());
            }
        }
        validate_calendar(&calendar).map_err(|err| err.to_string())?;

        let mut conn = self.open()?;
        let tx = conn.transaction()?;
        for event in events {
            upsert_event(&tx, calendar_id, event)?;
        }
        tx.commit()?;
        Ok(events.len())
    }

    pub fn record_import(
        &self,
        calendar_id: &str,
        source_url: Option<&str>,
        raw_text: &str,
        extracted_json: &str,
    ) -> StoreResult<i64> {
        self.init()?;
        let conn = self.open()?;
        conn.execute(
            "INSERT INTO imports (calendar_id, source_url, raw_text, extracted_json, created_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'))",
            params![calendar_id, source_url, raw_text, extracted_json],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn mark_import_applied(&self, import_id: i64) -> StoreResult<()> {
        self.init()?;
        let conn = self.open()?;
        conn.execute(
            "UPDATE imports SET applied_at = datetime('now') WHERE id = ?1",
            [import_id],
        )?;
        Ok(())
    }

    fn open(&self) -> StoreResult<Connection> {
        Ok(Connection::open(&self.path)?)
    }
}

fn init_schema(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        "
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS calendars (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            year INTEGER NOT NULL,
            region TEXT NOT NULL,
            exam_type TEXT NOT NULL,
            timezone TEXT NOT NULL,
            updated_at TEXT,
            source_name TEXT,
            source_url TEXT,
            default_alarm_minutes_json TEXT NOT NULL DEFAULT '[]'
        );

        CREATE TABLE IF NOT EXISTS events (
            calendar_id TEXT NOT NULL,
            id TEXT NOT NULL,
            title TEXT NOT NULL,
            start TEXT NOT NULL,
            end TEXT NOT NULL,
            all_day INTEGER NOT NULL,
            description TEXT,
            location TEXT,
            url TEXT,
            source_name TEXT,
            source_url TEXT,
            status TEXT,
            alarm_minutes_json TEXT,
            PRIMARY KEY (calendar_id, id),
            FOREIGN KEY (calendar_id) REFERENCES calendars(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS imports (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            calendar_id TEXT NOT NULL,
            source_url TEXT,
            raw_text TEXT NOT NULL,
            extracted_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            applied_at TEXT,
            FOREIGN KEY (calendar_id) REFERENCES calendars(id) ON DELETE CASCADE
        );
        ",
    )?;
    Ok(())
}

fn upsert_calendar(conn: &Connection, calendar: &CalendarFile) -> StoreResult<()> {
    let (source_name, source_url) = source_columns(calendar.source.as_ref());
    let alarms = serde_json::to_string(&calendar.default_alarm_minutes)?;
    conn.execute(
        "INSERT INTO calendars
            (id, title, year, region, exam_type, timezone, updated_at,
             source_name, source_url, default_alarm_minutes_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(id) DO UPDATE SET
            title = excluded.title,
            year = excluded.year,
            region = excluded.region,
            exam_type = excluded.exam_type,
            timezone = excluded.timezone,
            updated_at = excluded.updated_at,
            source_name = excluded.source_name,
            source_url = excluded.source_url,
            default_alarm_minutes_json = excluded.default_alarm_minutes_json",
        params![
            calendar.id,
            calendar.title,
            calendar.year,
            calendar.region,
            calendar.exam_type,
            calendar.timezone,
            calendar.updated_at,
            source_name,
            source_url,
            alarms,
        ],
    )?;
    Ok(())
}

fn upsert_event(conn: &Connection, calendar_id: &str, event: &Event) -> StoreResult<()> {
    let (source_name, source_url) = source_columns(event.source.as_ref());
    let status = event.status.as_ref().map(EventStatus::as_json_value);
    let alarms = match &event.alarm_minutes {
        Some(value) => Some(serde_json::to_string(value)?),
        None => None,
    };

    conn.execute(
        "INSERT INTO events
            (calendar_id, id, title, start, end, all_day, description, location,
             url, source_name, source_url, status, alarm_minutes_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(calendar_id, id) DO UPDATE SET
            title = excluded.title,
            start = excluded.start,
            end = excluded.end,
            all_day = excluded.all_day,
            description = excluded.description,
            location = excluded.location,
            url = excluded.url,
            source_name = excluded.source_name,
            source_url = excluded.source_url,
            status = excluded.status,
            alarm_minutes_json = excluded.alarm_minutes_json",
        params![
            calendar_id,
            event.id,
            event.title,
            event.start,
            event.end,
            if event.all_day { 1 } else { 0 },
            event.description,
            event.location,
            event.url,
            source_name,
            source_url,
            status,
            alarms,
        ],
    )?;
    Ok(())
}

fn load_events(conn: &Connection, calendar_id: &str) -> StoreResult<Vec<Event>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, start, end, all_day, description, location, url,
                source_name, source_url, status, alarm_minutes_json
         FROM events
         WHERE calendar_id = ?1
         ORDER BY start, id",
    )?;
    let rows = stmt.query_map([calendar_id], |row| {
        let source_name: Option<String> = row.get(8)?;
        let source_url: Option<String> = row.get(9)?;
        let status: Option<String> = row.get(10)?;
        let alarm_json: Option<String> = row.get(11)?;
        Ok(Event {
            id: row.get(0)?,
            title: row.get(1)?,
            start: row.get(2)?,
            end: row.get(3)?,
            all_day: row.get::<_, i64>(4)? != 0,
            description: row.get(5)?,
            location: row.get(6)?,
            url: row.get(7)?,
            source: source_from_columns(source_name, source_url),
            status: status
                .as_deref()
                .map(EventStatus::try_from)
                .transpose()
                .map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        10,
                        rusqlite::types::Type::Text,
                        err.into(),
                    )
                })?,
            alarm_minutes: alarm_json
                .as_deref()
                .map(parse_i64_vec)
                .transpose()
                .map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(11, rusqlite::types::Type::Text, err)
                })?,
        })
    })?;

    let mut events = Vec::new();
    for row in rows {
        events.push(row?);
    }
    Ok(events)
}

fn parse_i64_vec(value: &str) -> Result<Vec<i64>, BoxError> {
    Ok(serde_json::from_str(value)?)
}

fn source_columns(source: Option<&Source>) -> (Option<&str>, Option<&str>) {
    match source {
        Some(source) => (Some(source.name.as_str()), Some(source.url.as_str())),
        None => (None, None),
    }
}

fn source_from_columns(name: Option<String>, url: Option<String>) -> Option<Source> {
    match (name, url) {
        (Some(name), Some(url)) => Some(Source { name, url }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("exam-calendar-{name}-{stamp}"))
    }

    fn sample_calendar() -> CalendarFile {
        CalendarFile {
            id: "test-gaokao".to_string(),
            title: "测试高考日程 2026".to_string(),
            year: 2026,
            region: "CN-TS".to_string(),
            exam_type: "gaokao".to_string(),
            timezone: "Asia/Shanghai".to_string(),
            updated_at: Some("2026-03-16T00:00:00Z".to_string()),
            source: Some(Source {
                name: "测试来源".to_string(),
                url: "https://example.com".to_string(),
            }),
            default_alarm_minutes: vec![1440],
            events: vec![Event {
                id: "main".to_string(),
                title: "测试考试".to_string(),
                start: "2026-06-07".to_string(),
                end: "2026-06-10".to_string(),
                all_day: true,
                description: Some("测试说明".to_string()),
                location: None,
                url: Some("https://example.com".to_string()),
                source: None,
                status: Some(EventStatus::Confirmed),
                alarm_minutes: Some(vec![60]),
            }],
        }
    }

    #[test]
    fn saves_and_loads_calendar() {
        let db_path = temp_path("save-load.sqlite");
        let store = Store::new(&db_path);
        let calendar = sample_calendar();

        store.save_calendar(&calendar).unwrap();
        let loaded = store.load_calendar("test-gaokao").unwrap();

        assert_eq!(loaded.id, calendar.id);
        assert_eq!(loaded.events.len(), 1);
        assert_eq!(loaded.events[0].id, "main");
        assert_eq!(loaded.events[0].alarm_minutes, Some(vec![60]));
        let _ = fs::remove_file(db_path);
    }

    #[test]
    fn rejects_invalid_event_before_writing() {
        let db_path = temp_path("reject.sqlite");
        let store = Store::new(&db_path);
        store.save_calendar(&sample_calendar()).unwrap();

        let invalid = Event {
            id: "bad".to_string(),
            title: "坏日期".to_string(),
            start: "2026-06-07".to_string(),
            end: "2026-06-07".to_string(),
            all_day: true,
            description: None,
            location: None,
            url: None,
            source: None,
            status: None,
            alarm_minutes: None,
        };

        assert!(store.upsert_events("test-gaokao", &[invalid]).is_err());
        let loaded = store.load_calendar("test-gaokao").unwrap();
        assert_eq!(loaded.events.len(), 1);
        assert_eq!(loaded.events[0].id, "main");
        let _ = fs::remove_file(db_path);
    }
}
