//! Chat persistence: chat messages live in SQLite so a rejoin (or a late
//! joiner) can scroll back through what was said before they arrived.
//!
//! Sessions persist each message they relay and replay the most recent
//! history right after a successful join, in order, through the same
//! outbound queue the engine uses — so the client sees `Welcome` first
//! and then a run of `Chat` messages.

use huddle_protocol::ChatMessage;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// How much history a fresh join replays. Chat is a side channel; a
/// bounded replay keeps the join fast and the protocol message stream
/// small even in long-running meetings.
pub const HISTORY_LIMIT: i64 = 50;

/// Best-effort insert; the session logs and moves on if it fails. Losing
/// history is sad, dropping the relay because of it would be sadder.
pub async fn persist(pool: &SqlitePool, meeting_id: Uuid, message: &ChatMessage) {
    let Ok(sent_at) = message.sent_at.format(&Rfc3339) else {
        tracing::warn!("chat persist failed: unformattable timestamp");
        return;
    };
    let result = sqlx::query(
        "INSERT INTO chat_messages (id, meeting_id, sender_id, sender_name, body, sent_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(message.id.to_string())
    .bind(meeting_id.to_string())
    .bind(message.sender_id.map(|id| id.to_string()))
    .bind(&message.sender_name)
    .bind(&message.body)
    .bind(sent_at)
    .execute(pool)
    .await;
    if let Err(e) = result {
        tracing::warn!("chat persist failed: {e}");
    }
}

/// The most recent messages of a meeting, oldest first.
pub async fn history(pool: &SqlitePool, meeting_id: Uuid) -> Result<Vec<ChatMessage>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, sender_id, sender_name, body, sent_at
         FROM chat_messages WHERE meeting_id = ?1
         ORDER BY sent_at ASC, id ASC LIMIT ?2",
    )
    .bind(meeting_id.to_string())
    .bind(HISTORY_LIMIT)
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_message).collect()
}

fn row_to_message(row: &SqliteRow) -> Result<ChatMessage, sqlx::Error> {
    let id = Uuid::parse_str(row.try_get::<String, _>("id")?.as_str())
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let sender_id = row
        .try_get::<Option<String>, _>("sender_id")?
        .map(|raw| Uuid::parse_str(&raw))
        .transpose()
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let sent_at = OffsetDateTime::parse(row.try_get::<String, _>("sent_at")?.as_str(), &Rfc3339)
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    Ok(ChatMessage {
        id,
        sender_id,
        sender_name: row.try_get("sender_name")?,
        body: row.try_get("body")?,
        sent_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn history_round_trips_in_order_with_optional_sender() {
        let pool = crate::db::test_pool().await;
        let meeting_id = Uuid::new_v4();
        // The meetings table has a foreign key on chat_messages; give the
        // message a real meeting to point at.
        let now = OffsetDateTime::now_utc();
        sqlx::query(
            "INSERT INTO meetings (id, code, title, starts_at, ends_at, cancelled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
        )
        .bind(meeting_id.to_string())
        .bind("AAAA-BBBB-CCCC")
        .bind("test")
        .bind(now.format(&Rfc3339).unwrap())
        .bind((now + time::Duration::hours(1)).format(&Rfc3339).unwrap())
        .bind(now.format(&Rfc3339).unwrap())
        .execute(&pool)
        .await
        .unwrap();

        let system = ChatMessage {
            id: Uuid::new_v4(),
            sender_id: None,
            sender_name: "system".into(),
            body: "meeting started".into(),
            sent_at: now,
        };
        let user = ChatMessage {
            id: Uuid::new_v4(),
            sender_id: Some(Uuid::new_v4()),
            sender_name: "alice".into(),
            body: "hello".into(),
            sent_at: now + time::Duration::seconds(1),
        };
        persist(&pool, meeting_id, &system).await;
        persist(&pool, meeting_id, &user).await;

        let got = history(&pool, meeting_id).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].body, "meeting started");
        assert!(got[0].sender_id.is_none());
        assert_eq!(got[1].body, "hello");
        assert_eq!(got[1].sender_name, "alice");
        assert!(got[1].sender_id.is_some());
    }
}
