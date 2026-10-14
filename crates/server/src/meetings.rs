//! Meeting persistence and scheduling rules: creation, code generation,
//! the join window, and cancellation.

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use huddle_protocol::{CreateMeetingRequest, Joinability, MeetingDto, UpdateMeetingRequest};

use crate::config::Config;

/// One meeting row, domain-typed (UTC times, parsed codes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meeting {
    pub id: Uuid,
    pub code: String,
    pub title: String,
    pub starts_at: OffsetDateTime,
    pub ends_at: OffsetDateTime,
    pub cancelled: bool,
    pub created_at: OffsetDateTime,
}

impl From<Meeting> for MeetingDto {
    fn from(m: Meeting) -> Self {
        Self {
            id: m.id,
            code: m.code,
            title: m.title,
            starts_at: m.starts_at,
            ends_at: m.ends_at,
            cancelled: m.cancelled,
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
        cancelled: row.try_get::<i64, _>("cancelled")? != 0,
        created_at: from_text(
            row.try_get::<String, _>("created_at")?.as_str(),
            "created_at",
        )?,
    })
}

/// Create a meeting with a fresh, collision-checked code.
pub async fn create(
    pool: &SqlitePool,
    req: &CreateMeetingRequest,
    now: OffsetDateTime,
) -> Result<Meeting, MeetingError> {
    if req.validate().is_err() {
        return Err(MeetingError::Invalid);
    }

    let starts_at = to_text(req.starts_at).map_err(|e| MeetingError::Corrupt(e.to_string()))?;
    let ends_at = to_text(req.ends_at).map_err(|e| MeetingError::Corrupt(e.to_string()))?;
    let created_at = to_text(now).map_err(|e| MeetingError::Corrupt(e.to_string()))?;

    // Codes are random; a collision burns the insert on the UNIQUE index,
    // so just retry with a fresh code.
    for _ in 0..5 {
        let code = generate_code();
        let result = sqlx::query(
            "INSERT INTO meetings (id, code, title, starts_at, ends_at, cancelled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&code)
        .bind(req.title.trim())
        .bind(&starts_at)
        .bind(&ends_at)
        .bind(&created_at)
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
        "SELECT id, code, title, starts_at, ends_at, cancelled, created_at
         FROM meetings WHERE code = ?1",
    )
    .bind(normalize_code(code))
    .fetch_optional(pool)
    .await?;
    row.map(|r| row_to_meeting(&r)).transpose()
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
        let title = title.trim();
        if title.is_empty() || title.len() > 200 {
            return Err(MeetingError::Invalid);
        }
        meeting.title = title.to_owned();
    }
    if let Some(starts_at) = req.starts_at {
        meeting.starts_at = starts_at;
    }
    if let Some(ends_at) = req.ends_at {
        meeting.ends_at = ends_at;
    }
    if meeting.ends_at <= meeting.starts_at {
        return Err(MeetingError::Invalid);
    }

    let starts_at = to_text(meeting.starts_at).map_err(|e| MeetingError::Corrupt(e.to_string()))?;
    let ends_at = to_text(meeting.ends_at).map_err(|e| MeetingError::Corrupt(e.to_string()))?;
    sqlx::query("UPDATE meetings SET title = ?1, starts_at = ?2, ends_at = ?3 WHERE code = ?4")
        .bind(&meeting.title)
        .bind(starts_at)
        .bind(ends_at)
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

/// Joinability of a meeting at `now`, honoring the configured window.
pub fn joinability(meeting: &Meeting, config: &Config, now: OffsetDateTime) -> Joinability {
    if meeting.cancelled {
        return Joinability::Cancelled;
    }
    let (opens_at, closes_at) = config.join_window(meeting.starts_at, meeting.ends_at);
    if now < opens_at {
        Joinability::NotYet { opens_at }
    } else if now <= closes_at {
        Joinability::Open
    } else {
        Joinability::Ended
    }
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
        }
    }

    #[tokio::test]
    async fn create_assigns_code_and_round_trips() {
        let pool = pool().await;
        let meeting = create(
            &pool,
            &req(Duration::hours(1), Duration::hours(1), "Standup"),
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
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
        let lower = meeting.code.to_ascii_lowercase();
        let fetched = get_by_code(&pool, &lower).await.unwrap().unwrap();
        assert_eq!(fetched.id, meeting.id);
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
            },
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
    async fn cancel_marks_meeting() {
        let pool = pool().await;
        let meeting = create(
            &pool,
            &req(Duration::hours(1), Duration::hours(1), "1:1"),
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
            cancelled: false,
            created_at: start,
        };
        assert!(matches!(
            joinability(&meeting, &config, start - Duration::minutes(6)),
            Joinability::NotYet { .. }
        ));
        assert_eq!(
            joinability(&meeting, &config, start - Duration::minutes(4)),
            Joinability::Open
        );
        assert_eq!(
            joinability(&meeting, &config, start + Duration::hours(2)),
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
            cancelled: true,
            created_at: OffsetDateTime::now_utc(),
        };
        assert_eq!(
            joinability(&meeting, &config, OffsetDateTime::now_utc()),
            Joinability::Cancelled
        );
    }
}
