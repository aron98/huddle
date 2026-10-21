//! Meeting persistence and scheduling rules: creation, code generation,
//! the join window, and cancellation.

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use huddle_protocol::{
    CreateMeetingRequest, Joinability, MeetingDto, OccurrenceDto, RecurrenceRule,
    UpdateMeetingRequest,
};

use crate::config::Config;
use crate::recurrence;

/// One meeting row, domain-typed (UTC times, parsed codes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meeting {
    pub id: Uuid,
    pub code: String,
    pub title: String,
    pub starts_at: OffsetDateTime,
    pub ends_at: OffsetDateTime,
    /// The organizer who created the meeting, if any. `None` meetings
    /// are anonymous: the link is the only capability, anyone may edit.
    pub organizer_id: Option<Uuid>,
    pub cancelled: bool,
    pub created_at: OffsetDateTime,
    /// The series rule when this meeting recurs; `None` for a one-off.
    /// `starts_at` is the first occurrence.
    pub repeat: Option<RecurrenceRule>,
    /// Per-meeting join passcode (issue #13). `None` means open.
    pub passcode: Option<String>,
    /// End-to-end encrypted trust mode (issue #27), fixed at creation.
    pub e2ee: bool,
}

impl From<Meeting> for MeetingDto {
    fn from(m: Meeting) -> Self {
        // Issue #29: an encrypted stored title is never sent as-is in
        // `title` — viewers see the masked label, and the blob travels
        // in the additive `title_ciphertext` field for clients that can
        // decrypt it (the organizer's browser). This one impl covers
        // REST responses and the WS `Welcome` alike.
        let (title, title_ciphertext) = if huddle_protocol::is_encrypted_title(&m.title) {
            (
                huddle_protocol::MASKED_TITLE.to_owned(),
                Some(m.title.clone()),
            )
        } else {
            (m.title, None)
        };
        Self {
            id: m.id,
            code: m.code,
            title,
            starts_at: m.starts_at,
            ends_at: m.ends_at,
            cancelled: m.cancelled,
            repeat: m.repeat,
            has_passcode: m.passcode.is_some(),
            e2ee: m.e2ee,
            title_ciphertext,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MeetingError {
    #[error("no meeting with that code")]
    NotFound,
    #[error("times or title are invalid")]
    Invalid,
    #[error("could not generate a unique meeting code")]
    CodeCollision,
    #[error("storage failure: {0}")]
    Storage(#[from] sqlx::Error),
    #[error("corrupt row: {0}")]
    Corrupt(String),
}

/// Generate a readable meeting code like `H7KD-PM2X-QR4T`.
///
/// The alphabet drops ambiguous characters (I, L, O, U, 0, 1) so codes stay
/// typeable over the phone; codes are the capability, so keep them long
/// enough to resist guessing (31^12 ≈ 7.9e17).
const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

fn generate_code() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let mut code = String::with_capacity(14);
    for i in 0..12 {
        if i > 0 && i % 4 == 0 {
            code.push('-');
        }
        code.push(ALPHABET[rng.random_range(0..ALPHABET.len())] as char);
    }
    code
}

/// Normalize a code as typed into a URL (`huddle.example/m/CODE`).
pub fn normalize_code(raw: &str) -> String {
    raw.trim().trim_start_matches('/').to_ascii_uppercase()
}

pub(crate) fn to_text(t: OffsetDateTime) -> anyhow::Result<String> {
    Ok(t.to_utc().format(&Rfc3339)?)
}

pub(crate) fn from_text(raw: &str, column: &str) -> Result<OffsetDateTime, MeetingError> {
    OffsetDateTime::parse(raw, &Rfc3339)
        .map_err(|e| MeetingError::Corrupt(format!("{column}: {e}")))
}

fn row_to_meeting(row: &SqliteRow) -> Result<Meeting, MeetingError> {
    Ok(Meeting {
        id: Uuid::parse_str(row.try_get::<String, _>("id")?.as_str())
            .map_err(|e| MeetingError::Corrupt(format!("id: {e}")))?,
        code: row.try_get("code")?,
        title: row.try_get("title")?,
        starts_at: from_text(row.try_get::<String, _>("starts_at")?.as_str(), "starts_at")?,
        ends_at: from_text(row.try_get::<String, _>("ends_at")?.as_str(), "ends_at")?,
        organizer_id: row
            .try_get::<Option<String>, _>("organizer_id")?
            .map(|raw| Uuid::parse_str(&raw))
            .transpose()
            .map_err(|e| MeetingError::Corrupt(format!("organizer_id: {e}")))?,
        cancelled: row.try_get::<i64, _>("cancelled")? != 0,
        created_at: from_text(
            row.try_get::<String, _>("created_at")?.as_str(),
            "created_at",
        )?,
        repeat: row_repeat(row)?,
        passcode: row.try_get("passcode")?,
        e2ee: row.try_get::<i64, _>("e2ee")? != 0,
    })
}

/// Parse the recurrence columns. A NULL rule means a one-off (orphaned
/// `rrule_until`/`rrule_count` with no rule are ignored); a rule string
/// outside the closed set the server writes is corrupt.
fn row_repeat(row: &SqliteRow) -> Result<Option<RecurrenceRule>, MeetingError> {
    let Some(raw) = row.try_get::<Option<String>, _>("rrule")? else {
        return Ok(None);
    };
    let frequency = recurrence::parse_stored_rule(&raw)
        .ok_or_else(|| MeetingError::Corrupt(format!("rrule: {raw}")))?;
    let until = row
        .try_get::<Option<String>, _>("rrule_until")?
        .map(|raw| from_text(&raw, "rrule_until"))
        .transpose()?;
    let count = row
        .try_get::<Option<i64>, _>("rrule_count")?
        .map(i64::try_into)
        .transpose()
        .map_err(|_| MeetingError::Corrupt("rrule_count: out of range".into()))?;
    Ok(Some(RecurrenceRule {
        frequency,
        until,
        count,
    }))
}

/// The persisted form of a rule, split across the three `meetings`
/// columns: the closed-set `rrule` string, the RFC 3339 `rrule_until`,
/// and the `rrule_count`.
type StoredRepeat = (Option<String>, Option<String>, Option<i64>);

/// Serialize a rule into the three storage columns. The rule itself
/// has already been validated (`CreateMeetingRequest::validate` or the
/// explicit check in `update_by_code`).
fn repeat_columns(repeat: &Option<RecurrenceRule>) -> Result<StoredRepeat, MeetingError> {
    let Some(rule) = repeat else {
        return Ok((None, None, None));
    };
    let rrule = recurrence::stored_rule(rule.frequency).to_owned();
    let rrule_until = rule
        .until
        .map(to_text)
        .transpose()
        .map_err(|e| MeetingError::Corrupt(e.to_string()))?;
    Ok((Some(rrule), rrule_until, rule.count.map(i64::from)))
}

/// Create a meeting with a fresh, collision-checked code. `organizer_id`
/// attaches the meeting to a logged-in organizer; `None` leaves it
/// anonymous (the meeting link is the only capability, by design).
pub async fn create(
    pool: &SqlitePool,
    req: &CreateMeetingRequest,
    organizer_id: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<Meeting, MeetingError> {
    if req.validate().is_err() {
        return Err(MeetingError::Invalid);
    }

    let starts_at = to_text(req.starts_at).map_err(|e| MeetingError::Corrupt(e.to_string()))?;
    let ends_at = to_text(req.ends_at).map_err(|e| MeetingError::Corrupt(e.to_string()))?;
    let created_at = to_text(now).map_err(|e| MeetingError::Corrupt(e.to_string()))?;
    let (rrule, rrule_until, rrule_count) = repeat_columns(&req.repeat)?;

    // Codes are random; a collision burns the insert on the UNIQUE index,
    // so just retry with a fresh code.
    let passcode = req
        .passcode
        .as_ref()
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty());
    for _ in 0..5 {
        let code = generate_code();
        let result = sqlx::query(
            "INSERT INTO meetings (id, code, title, starts_at, ends_at, organizer_id, cancelled, created_at, rrule, rrule_until, rrule_count, passcode, e2ee)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8, ?9, ?10, ?11, ?12)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&code)
        .bind(req.title.trim())
        .bind(&starts_at)
        .bind(&ends_at)
        .bind(organizer_id.map(|id| id.to_string()))
        .bind(&created_at)
        .bind(&rrule)
        .bind(&rrule_until)
        .bind(rrule_count)
        .bind(&passcode)
        .bind(i64::from(req.e2ee))
        .execute(pool)
        .await;

        match result {
            Ok(_) => {
                return get_by_code(pool, &code)
                    .await?
                    .ok_or(MeetingError::NotFound);
            }
            Err(sqlx::Error::Database(db)) if db.message().contains("UNIQUE") => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(MeetingError::CodeCollision)
}

/// Look up a meeting by its (normalized) code.
pub async fn get_by_code(pool: &SqlitePool, code: &str) -> Result<Option<Meeting>, MeetingError> {
    let row = sqlx::query(
        "SELECT id, code, title, starts_at, ends_at, organizer_id, cancelled, created_at, rrule, rrule_until, rrule_count, passcode, e2ee
         FROM meetings WHERE code = ?1",
    )
    .bind(normalize_code(code))
    .fetch_optional(pool)
    .await?;
    row.map(|r| row_to_meeting(&r)).transpose()
}

/// All meetings created by one organizer, soonest first — the dashboard
/// list behind `GET /api/meetings/mine`.
pub async fn list_for_organizer(
    pool: &SqlitePool,
    organizer_id: Uuid,
) -> Result<Vec<Meeting>, MeetingError> {
    let rows = sqlx::query(
        "SELECT id, code, title, starts_at, ends_at, organizer_id, cancelled, created_at, rrule, rrule_until, rrule_count, passcode, e2ee
         FROM meetings WHERE organizer_id = ?1 ORDER BY starts_at ASC",
    )
    .bind(organizer_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_meeting).collect()
}

/// Apply a partial update. Unknown code → `None`.
pub async fn update_by_code(
    pool: &SqlitePool,
    code: &str,
    req: UpdateMeetingRequest,
) -> Result<Option<Meeting>, MeetingError> {
    let Some(mut meeting) = get_by_code(pool, code).await? else {
        return Ok(None);
    };

    if let Some(title) = &req.title {
        // Shared with create validation (issue #29): accepts either a
        // plaintext title or an `enc1.`-prefixed encrypted blob.
        huddle_protocol::validate_title(title).map_err(|_| MeetingError::Invalid)?;
        meeting.title = title.trim().to_owned();
    }
    if let Some(starts_at) = req.starts_at {
        meeting.starts_at = starts_at;
    }
    if let Some(ends_at) = req.ends_at {
        meeting.ends_at = ends_at;
    }
    // Tri-state passcode: absent leaves it alone, `null` removes it,
    // a value sets it (validated and trimmed like create).
    match req.passcode {
        None => {}
        Some(None) => meeting.passcode = None,
        Some(Some(passcode)) => {
            huddle_protocol::validate_passcode(&passcode).map_err(|_| MeetingError::Invalid)?;
            meeting.passcode = Some(passcode.trim().to_owned());
        }
    }
    if meeting.ends_at <= meeting.starts_at {
        return Err(MeetingError::Invalid);
    }
    // Absent leaves the rule unchanged; explicit `null` clears it back
    // to a one-off; a rule replaces it. Either way the rule is
    // validated against the (possibly just changed) series start.
    match req.repeat {
        None => {}
        Some(None) => meeting.repeat = None,
        Some(Some(rule)) => {
            if rule.validate(meeting.starts_at).is_err() {
                return Err(MeetingError::Invalid);
            }
            meeting.repeat = Some(rule);
        }
    }

    let starts_at = to_text(meeting.starts_at).map_err(|e| MeetingError::Corrupt(e.to_string()))?;
    let ends_at = to_text(meeting.ends_at).map_err(|e| MeetingError::Corrupt(e.to_string()))?;
    let (rrule, rrule_until, rrule_count) = repeat_columns(&meeting.repeat)?;
    sqlx::query(
        "UPDATE meetings SET title = ?1, starts_at = ?2, ends_at = ?3, rrule = ?4, rrule_until = ?5, rrule_count = ?6, passcode = ?7
         WHERE code = ?8",
    )
    .bind(&meeting.title)
    .bind(starts_at)
    .bind(ends_at)
    .bind(rrule)
    .bind(rrule_until)
    .bind(rrule_count)
    .bind(&meeting.passcode)
    .bind(&meeting.code)
    .execute(pool)
    .await?;

    Ok(Some(meeting))
}

/// Cancel or restore a meeting. Returns the updated row.
pub async fn set_cancelled(
    pool: &SqlitePool,
    code: &str,
    cancelled: bool,
) -> Result<Option<Meeting>, MeetingError> {
    sqlx::query("UPDATE meetings SET cancelled = ?1 WHERE code = ?2")
        .bind(i64::from(cancelled))
        .bind(normalize_code(code))
        .execute(pool)
        .await?;
    get_by_code(pool, code).await
}

/// Joinability of a (possibly recurring) meeting at `now`, honoring
/// per-occurrence cancellations. A one-off keeps the classic single
/// window; a series is joinable when `now` falls inside any open
/// occurrence's window. A cancelled occurrence only reports
/// `Cancelled` when it is the one happening now — a series with a
/// live future occurrence stays joinable-later, not cancelled.
pub fn joinability_with(
    meeting: &Meeting,
    cancelled_occurrences: &[OffsetDateTime],
    config: &Config,
    now: OffsetDateTime,
) -> Joinability {
    if meeting.cancelled {
        return Joinability::Cancelled;
    }
    let Some(rule) = &meeting.repeat else {
        let (opens_at, closes_at) = config.join_window(meeting.starts_at, meeting.ends_at);
        return single_window(opens_at, closes_at, now);
    };

    let duration = meeting.ends_at - meeting.starts_at;
    let mut cancelled_now = false;
    for start in recurrence::occurrence_starts(rule, meeting.starts_at) {
        let (opens_at, closes_at) = config.join_window(start, start + duration);
        if cancelled_occurrences.contains(&start) {
            if opens_at <= now && now <= closes_at {
                cancelled_now = true;
            }
            continue;
        }
        if now < opens_at {
            return Joinability::NotYet { opens_at };
        }
        if now <= closes_at {
            return Joinability::Open;
        }
    }
    if cancelled_now {
        Joinability::Cancelled
    } else {
        Joinability::Ended
    }
}

/// The single-meeting window logic, factored so both the one-off path
/// and each occurrence of a series use identical arithmetic.
fn single_window(
    opens_at: OffsetDateTime,
    closes_at: OffsetDateTime,
    now: OffsetDateTime,
) -> Joinability {
    if now < opens_at {
        Joinability::NotYet { opens_at }
    } else if now <= closes_at {
        Joinability::Open
    } else {
        Joinability::Ended
    }
}

/// The concrete upcoming occurrences of a series — not yet ended,
/// marked with their per-occurrence cancel state — up to `limit`.
/// One-off meetings have none.
pub fn upcoming_occurrences(
    meeting: &Meeting,
    cancelled: &[OffsetDateTime],
    now: OffsetDateTime,
    limit: usize,
) -> Vec<OccurrenceDto> {
    let Some(rule) = &meeting.repeat else {
        return Vec::new();
    };
    let duration = meeting.ends_at - meeting.starts_at;
    recurrence::occurrence_starts(rule, meeting.starts_at)
        .into_iter()
        .map(|start| OccurrenceDto {
            starts_at: start,
            ends_at: start + duration,
            cancelled: cancelled.contains(&start),
        })
        .filter(|occurrence| occurrence.ends_at >= now)
        .take(limit)
        .collect()
}

/// The cancelled occurrence starts of a meeting series, ascending.
pub async fn cancelled_occurrences(
    pool: &SqlitePool,
    meeting_id: Uuid,
) -> Result<Vec<OffsetDateTime>, MeetingError> {
    let rows = sqlx::query(
        "SELECT starts_at FROM cancelled_occurrences WHERE meeting_id = ?1 ORDER BY starts_at ASC",
    )
    .bind(meeting_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            from_text(
                row.try_get::<String, _>("starts_at")?.as_str(),
                "occurrence starts_at",
            )
        })
        .collect()
}

/// Cancel a single occurrence of a recurring series: the date joins
/// the series' cancelled set, the series itself stays intact.
/// `None` for an unknown code. The occurrence must be a real start
/// time of the series (clients send back exactly what
/// [`upcoming_occurrences`] handed them).
pub async fn cancel_occurrence(
    pool: &SqlitePool,
    code: &str,
    occurrence: OffsetDateTime,
) -> Result<Option<Vec<OffsetDateTime>>, MeetingError> {
    let Some(meeting) = get_by_code(pool, code).await? else {
        return Ok(None);
    };
    let Some(rule) = &meeting.repeat else {
        return Err(MeetingError::Invalid);
    };
    if !recurrence::occurrence_starts(rule, meeting.starts_at).contains(&occurrence) {
        return Err(MeetingError::Invalid);
    }
    sqlx::query(
        "INSERT OR IGNORE INTO cancelled_occurrences (meeting_id, starts_at) VALUES (?1, ?2)",
    )
    .bind(meeting.id.to_string())
    .bind(to_text(occurrence).map_err(|e| MeetingError::Corrupt(e.to_string()))?)
    .execute(pool)
    .await?;
    Ok(Some(cancelled_occurrences(pool, meeting.id).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;
    use time::Duration;

    async fn pool() -> SqlitePool {
        test_pool().await
    }

    fn req(starts_in: Duration, lasts: Duration, title: &str) -> CreateMeetingRequest {
        let now = OffsetDateTime::now_utc();
        CreateMeetingRequest {
            title: title.into(),
            starts_at: now + starts_in,
            ends_at: now + starts_in + lasts,
            repeat: None,
            passcode: None,
            e2ee: false,
        }
    }

    #[tokio::test]
    async fn create_assigns_code_and_round_trips() {
        let pool = pool().await;
        let meeting = create(
            &pool,
            &req(Duration::hours(1), Duration::hours(1), "Standup"),
            None,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        assert_eq!(meeting.title, "Standup");
        assert_eq!(meeting.code.len(), 14);
        assert_eq!(meeting.code.matches('-').count(), 2);
        assert!(meeting.code.split('-').all(|g| g.len() == 4));

        let fetched = get_by_code(&pool, &meeting.code).await.unwrap().unwrap();
        assert_eq!(fetched.id, meeting.id);
        assert!(!fetched.cancelled);
    }

    #[tokio::test]
    async fn code_lookup_is_normalized() {
        let pool = pool().await;
        let meeting = create(
            &pool,
            &req(Duration::hours(1), Duration::hours(1), "Retro"),
            None,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        let lower = meeting.code.to_ascii_lowercase();
        let fetched = get_by_code(&pool, &lower).await.unwrap().unwrap();
        assert_eq!(fetched.id, meeting.id);
    }

    #[tokio::test]
    async fn e2ee_flag_round_trips_through_storage() {
        let pool = pool().await;
        let mut request = req(Duration::hours(1), Duration::hours(1), "Secret sync");
        request.e2ee = true;
        let meeting = create(&pool, &request, None, OffsetDateTime::now_utc())
            .await
            .unwrap();
        assert!(meeting.e2ee);

        let fetched = get_by_code(&pool, &meeting.code).await.unwrap().unwrap();
        assert!(fetched.e2ee, "the flag survives storage");
        assert_eq!(fetched.id, meeting.id);

        // The DTO carries the trust mode to clients.
        let dto = MeetingDto::from(fetched);
        assert!(dto.e2ee);

        // Plain meetings stay plain.
        let plain = create(
            &pool,
            &req(Duration::hours(1), Duration::hours(1), "Open sync"),
            None,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        assert!(!plain.e2ee);
        assert!(!get_by_code(&pool, &plain.code).await.unwrap().unwrap().e2ee);
    }

    #[tokio::test]
    async fn create_rejects_backwards_times() {
        let pool = pool().await;
        let err = create(
            &pool,
            &CreateMeetingRequest {
                title: "Broken".into(),
                starts_at: OffsetDateTime::now_utc() + Duration::hours(2),
                ends_at: OffsetDateTime::now_utc() + Duration::hours(1),
                repeat: None,
                passcode: None,
                e2ee: false,
            },
            None,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, MeetingError::Invalid));
    }

    #[tokio::test]
    async fn update_changes_title_and_times() {
        let pool = pool().await;
        let meeting = create(
            &pool,
            &req(Duration::hours(1), Duration::hours(1), "Old title"),
            None,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        let new_start = meeting.starts_at + Duration::minutes(30);
        let updated = update_by_code(
            &pool,
            &meeting.code,
            UpdateMeetingRequest {
                title: Some("New title".into()),
                starts_at: Some(new_start),
                ends_at: None,
                cancelled: None,
                repeat: None,
                passcode: None,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.title, "New title");
        assert_eq!(updated.starts_at, new_start);
        assert_eq!(updated.ends_at, meeting.ends_at);
    }

    #[tokio::test]
    async fn create_stores_trimmed_passcode() {
        let pool = pool().await;
        let mut request = req(Duration::hours(1), Duration::hours(1), "Locked sync");
        request.passcode = Some("  sekrit  ".into());
        let meeting = create(&pool, &request, None, OffsetDateTime::now_utc())
            .await
            .unwrap();
        assert_eq!(meeting.passcode.as_deref(), Some("sekrit"));

        // The DTO exposes only the fact that there is one.
        let dto = MeetingDto::from(meeting.clone());
        assert!(dto.has_passcode);

        let fetched = get_by_code(&pool, &meeting.code).await.unwrap().unwrap();
        assert_eq!(fetched.passcode.as_deref(), Some("sekrit"));
    }

    #[tokio::test]
    async fn create_rejects_blank_passcode() {
        let pool = pool().await;
        let mut request = req(Duration::hours(1), Duration::hours(1), "Broken lock");
        request.passcode = Some("   ".into());
        let err = create(&pool, &request, None, OffsetDateTime::now_utc())
            .await
            .unwrap_err();
        assert!(matches!(err, MeetingError::Invalid));
    }

    #[tokio::test]
    async fn update_passcode_is_tri_state() {
        let pool = pool().await;
        let meeting = create(
            &pool,
            &req(Duration::hours(1), Duration::hours(1), "Locked"),
            None,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        assert_eq!(meeting.passcode, None);

        // Absent leaves the passcode unchanged (still none).
        let updated = update_by_code(
            &pool,
            &meeting.code,
            UpdateMeetingRequest {
                title: Some("Retitled".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.passcode, None);

        // A value sets it.
        let updated = update_by_code(
            &pool,
            &meeting.code,
            UpdateMeetingRequest {
                passcode: Some(Some("sekrit".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.passcode.as_deref(), Some("sekrit"));

        // Absent still leaves it alone.
        let updated = update_by_code(
            &pool,
            &meeting.code,
            UpdateMeetingRequest {
                title: Some("Again".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.passcode.as_deref(), Some("sekrit"));

        // Explicit null removes it.
        let updated = update_by_code(
            &pool,
            &meeting.code,
            UpdateMeetingRequest {
                passcode: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.passcode, None);

        // A blank value is rejected as invalid.
        let err = update_by_code(
            &pool,
            &meeting.code,
            UpdateMeetingRequest {
                passcode: Some(Some("  ".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, MeetingError::Invalid));
    }

    #[tokio::test]
    async fn cancel_marks_meeting() {
        let pool = pool().await;
        let meeting = create(
            &pool,
            &req(Duration::hours(1), Duration::hours(1), "1:1"),
            None,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        let cancelled = set_cancelled(&pool, &meeting.code, true)
            .await
            .unwrap()
            .unwrap();
        assert!(cancelled.cancelled);
    }

    #[tokio::test]
    async fn owned_meetings_list_for_their_organizer() {
        let pool = pool().await;

        // organizers.organizer_id is a foreign key: stand up real rows.
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();
        for (id, email) in [(alice, "alice@example.com"), (bob, "bob@example.com")] {
            sqlx::query("INSERT INTO organizers (id, email, created_at) VALUES (?1, ?2, ?3)")
                .bind(id.to_string())
                .bind(email)
                .bind(to_text(OffsetDateTime::now_utc()).unwrap())
                .execute(&pool)
                .await
                .unwrap();
        }

        let owned = create(
            &pool,
            &req(Duration::hours(1), Duration::hours(1), "Alice's sync"),
            Some(alice),
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        assert_eq!(owned.organizer_id, Some(alice));

        // Visible through the dashboard query, invisible to anyone else.
        let mine = list_for_organizer(&pool, alice).await.unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].id, owned.id);

        let theirs = list_for_organizer(&pool, bob).await.unwrap();
        assert!(theirs.is_empty());
    }

    #[tokio::test]
    async fn unknown_code_is_none() {
        let pool = pool().await;
        assert!(
            get_by_code(&pool, "ZZZZ-ZZZZ-ZZZZ")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn joinability_follows_window() {
        let config = Config::default();
        let start = OffsetDateTime::now_utc();
        let meeting = Meeting {
            id: Uuid::new_v4(),
            code: generate_code(),
            title: "test".into(),
            starts_at: start,
            ends_at: start + Duration::hours(1),
            organizer_id: None,
            cancelled: false,
            created_at: start,
            repeat: None,
            passcode: None,
            e2ee: false,
        };
        assert!(matches!(
            joinability_with(&meeting, &[], &config, start - Duration::minutes(6)),
            Joinability::NotYet { .. }
        ));
        assert_eq!(
            joinability_with(&meeting, &[], &config, start - Duration::minutes(4)),
            Joinability::Open
        );
        assert_eq!(
            joinability_with(&meeting, &[], &config, start + Duration::hours(2)),
            Joinability::Ended
        );
    }

    #[test]
    fn joinability_cancelled_wins() {
        let config = Config::default();
        let meeting = Meeting {
            id: Uuid::new_v4(),
            code: generate_code(),
            title: "test".into(),
            starts_at: OffsetDateTime::now_utc(),
            ends_at: OffsetDateTime::now_utc() + Duration::hours(1),
            organizer_id: None,
            cancelled: true,
            created_at: OffsetDateTime::now_utc(),
            repeat: None,
            passcode: None,
            e2ee: false,
        };
        assert_eq!(
            joinability_with(&meeting, &[], &config, OffsetDateTime::now_utc()),
            Joinability::Cancelled
        );
    }

    // --- recurring series ---------------------------------------------------

    use huddle_protocol::RepeatFrequency;

    /// A weekly series starting at `starts_at` with a one-hour slot.
    fn series(starts_at: OffsetDateTime, count: Option<u32>) -> Meeting {
        Meeting {
            id: Uuid::new_v4(),
            code: generate_code(),
            title: "test".into(),
            starts_at,
            ends_at: starts_at + Duration::hours(1),
            organizer_id: None,
            cancelled: false,
            created_at: OffsetDateTime::now_utc(),
            passcode: None,
            e2ee: false,
            repeat: Some(RecurrenceRule {
                frequency: RepeatFrequency::Weekly,
                until: None,
                count,
            }),
        }
    }

    fn weekly_req(
        starts_in: Duration,
        lasts: Duration,
        count: Option<u32>,
    ) -> CreateMeetingRequest {
        let now = OffsetDateTime::now_utc();
        CreateMeetingRequest {
            title: "Series".into(),
            starts_at: now + starts_in,
            ends_at: now + starts_in + lasts,
            passcode: None,
            e2ee: false,
            repeat: Some(RecurrenceRule {
                frequency: RepeatFrequency::Weekly,
                until: None,
                count,
            }),
        }
    }

    #[test]
    fn recurring_series_opens_at_the_current_occurrence() {
        let config = Config::default();
        let now = OffsetDateTime::now_utc();
        // Started a week and ten minutes ago: the second occurrence is
        // happening now (it began ten minutes ago, so its window —
        // opening five minutes early — is open).
        let meeting = series(now - Duration::days(7) - Duration::minutes(10), None);
        assert_eq!(
            joinability_with(&meeting, &[], &config, now),
            Joinability::Open
        );
    }

    #[test]
    fn recurring_series_waits_between_occurrences() {
        let config = Config::default();
        let now = OffsetDateTime::now_utc();
        // Three days into a weekly series: the first occurrence is
        // over, the next is four days out.
        let meeting = series(now - Duration::days(3), None);
        let next_start = now + Duration::days(4);
        assert_eq!(
            joinability_with(&meeting, &[], &config, now),
            Joinability::NotYet {
                opens_at: next_start - Duration::minutes(5),
            }
        );
    }

    #[test]
    fn recurring_series_ends_after_the_last_occurrence() {
        let config = Config::default();
        let now = OffsetDateTime::now_utc();
        // A single-occurrence series whose window closed long ago.
        let meeting = series(now - Duration::days(7), Some(1));
        assert_eq!(
            joinability_with(&meeting, &[], &config, now),
            Joinability::Ended
        );
    }

    #[test]
    fn cancelled_current_occurrence_without_a_future_one_is_cancelled() {
        let config = Config::default();
        let now = OffsetDateTime::now_utc();
        // Happening now, and it is the only occurrence.
        let start = now - Duration::minutes(10);
        let meeting = series(start, Some(1));
        assert_eq!(
            joinability_with(&meeting, &[start], &config, now),
            Joinability::Cancelled
        );
    }

    #[test]
    fn cancelled_current_occurrence_with_a_future_one_waits() {
        let config = Config::default();
        let now = OffsetDateTime::now_utc();
        // The occurrence happening now is cancelled, but the series
        // continues: the next one is still in the future.
        let start = now - Duration::minutes(10);
        let meeting = series(start, None);
        assert!(matches!(
            joinability_with(&meeting, &[start], &config, now),
            Joinability::NotYet { .. }
        ));
    }

    #[test]
    fn upcoming_occurrences_start_after_now_and_mark_cancellations() {
        let now = OffsetDateTime::now_utc();
        // Two days into a weekly series: the in-flight occurrence has
        // already ended (start + 1h < now), so the first upcoming
        // occurrence is in five days; the cap keeps it at five.
        let start = now - Duration::days(2);
        let meeting = series(start, None);
        let cancelled = [start + Duration::days(7)];
        let upcoming = upcoming_occurrences(&meeting, &cancelled, now, 5);
        let expected: Vec<_> = (1..=5)
            .map(|weeks| start + Duration::days(7 * weeks))
            .collect();
        assert_eq!(
            upcoming.iter().map(|o| o.starts_at).collect::<Vec<_>>(),
            expected
        );
        assert!(upcoming[0].cancelled, "the cancelled one is marked");
        assert!(upcoming[1..].iter().all(|o| !o.cancelled));
        assert_eq!(
            upcoming[0].ends_at,
            start + Duration::days(7) + Duration::hours(1)
        );

        // One-off meetings have no upcoming occurrences.
        let single = Meeting {
            repeat: None,
            ..series(start, None)
        };
        assert!(upcoming_occurrences(&single, &[], now, 5).is_empty());
    }

    #[tokio::test]
    async fn repeat_round_trips_through_storage() {
        let pool = pool().await;
        let req = weekly_req(Duration::hours(1), Duration::hours(1), Some(5));
        let meeting = create(&pool, &req, None, OffsetDateTime::now_utc())
            .await
            .unwrap();
        assert_eq!(meeting.repeat, req.repeat);

        let fetched = get_by_code(&pool, &meeting.code).await.unwrap().unwrap();
        assert_eq!(fetched.repeat, req.repeat);
    }

    #[tokio::test]
    async fn update_sets_clears_and_replaces_the_recurrence() {
        let pool = pool().await;
        let meeting = create(
            &pool,
            &req(Duration::hours(1), Duration::hours(1), "One-off"),
            None,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        assert_eq!(meeting.repeat, None);

        // Absent: unchanged (the rule stays unset).
        let updated = update_by_code(
            &pool,
            &meeting.code,
            UpdateMeetingRequest {
                title: Some("Still one-off".into()),
                ..UpdateMeetingRequest::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.repeat, None);

        // Present: the series is set.
        let rule = RecurrenceRule {
            frequency: RepeatFrequency::Daily,
            until: None,
            count: Some(10),
        };
        let updated = update_by_code(
            &pool,
            &meeting.code,
            UpdateMeetingRequest {
                repeat: Some(Some(rule.clone())),
                ..UpdateMeetingRequest::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.repeat, Some(rule.clone()));

        // `null`: cleared back to a one-off.
        let updated = update_by_code(
            &pool,
            &meeting.code,
            UpdateMeetingRequest {
                repeat: Some(None),
                ..UpdateMeetingRequest::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.repeat, None);

        // A rule that excludes the series start is refused.
        let err = update_by_code(
            &pool,
            &meeting.code,
            UpdateMeetingRequest {
                repeat: Some(Some(RecurrenceRule {
                    frequency: RepeatFrequency::Weekly,
                    until: Some(updated.starts_at - Duration::hours(1)),
                    count: None,
                })),
                ..UpdateMeetingRequest::default()
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, MeetingError::Invalid));
    }

    #[tokio::test]
    async fn cancel_occurrence_marks_one_date_and_keeps_the_series() {
        let pool = pool().await;
        let meeting = create(
            &pool,
            &weekly_req(Duration::hours(1), Duration::hours(1), None),
            None,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        assert!(meeting.repeat.is_some());

        let cancelled = cancel_occurrence(&pool, &meeting.code, meeting.starts_at)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cancelled, vec![meeting.starts_at]);

        // Idempotent: the same date does not duplicate.
        let cancelled = cancel_occurrence(&pool, &meeting.code, meeting.starts_at)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cancelled, vec![meeting.starts_at]);

        // The series itself is untouched.
        let fetched = get_by_code(&pool, &meeting.code).await.unwrap().unwrap();
        assert!(!fetched.cancelled);
        assert!(fetched.repeat.is_some());
    }

    #[tokio::test]
    async fn cancel_occurrence_rejects_non_recurring_and_bad_dates() {
        let pool = pool().await;

        // A one-off has no occurrences to cancel.
        let single = create(
            &pool,
            &req(Duration::hours(1), Duration::hours(1), "One-off"),
            None,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        let err = cancel_occurrence(&pool, &single.code, single.starts_at)
            .await
            .unwrap_err();
        assert!(matches!(err, MeetingError::Invalid));

        // A date the series never lands on (off by one day).
        let recurring = create(
            &pool,
            &weekly_req(Duration::hours(1), Duration::hours(1), None),
            None,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        let err = cancel_occurrence(
            &pool,
            &recurring.code,
            recurring.starts_at + Duration::days(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, MeetingError::Invalid));

        // Unknown code: nothing to cancel, not an error.
        assert!(
            cancel_occurrence(&pool, "ZZZZ-ZZZZ-ZZZZ", OffsetDateTime::now_utc())
                .await
                .unwrap()
                .is_none()
        );
    }
}
