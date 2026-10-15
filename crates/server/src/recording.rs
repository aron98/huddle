//! Meeting recording (issue 15): the server-side sink.
//!
//! [`start`] spawns a writer thread and returns a [`Recorder`] for the
//! recording REST handler to hand to the engine, which installs it on
//! the live room. Frames flow engine-thread → bounded channel → writer;
//! the engine never touches disk. **Fail-open by construction**: a full
//! channel or a dead writer only drops frames — the meeting is never
//! blocked or crashed by recording trouble.
//!
//! On-disk layout, one directory per recording, nested per meeting:
//!
//! ```text
//! {data_dir}/recordings/{meeting_id}/{recording_id}/
//!   {participant}-audio.rtp   length-framed RTP packets
//!   {participant}-video.rtp
//!   {participant}-screen.rtp
//!   session.sdp               SDP manifest for the ffmpeg sidecar
//!   manifest.json             track table for the ffmpeg sidecar
//! ```
//!
//! Each `.rtp` packet record is `u16` big-endian length, then the 12-byte
//! RTP header and payload: RTP headers carry no payload length, so the
//! framing is what lets the sidecar split the stream back into UDP
//! datagrams.
//!
//! The ffmpeg sidecar (see `docker-compose.yml` and `deploy/recording`)
//! reads `manifest.json`, replays each `.rtp` file as UDP datagrams into
//! `ffmpeg -protocol_whitelist file,udp,rtp -f sdp -i session.sdp`, then
//! muxes the video tiles plus mixed audio into a single output file.
//!
//! Lifecycle: the engine drops the last `Arc` to the sink when recording
//! is stopped (explicitly) or when the meeting empties (the instance is
//! dropped). That closes the channel; the writer flushes its files,
//! writes the manifest, and finalizes the metadata row (`stopped_at`,
//! `size_bytes`) via the tokio handle captured at `start` time. The
//! writer only ever UPDATEs existing rows — the REST handler INSERTs
//! first and DELETEs on a failed start, so a late finalize can never
//! resurrect a rolled-back recording.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::Duration;

use huddle_protocol::RecordingDto;
use huddle_sfu::{RecordedFrame, RecorderSink};
use sqlx::{Row, SqlitePool};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::runtime::Handle;
use uuid::Uuid;

/// Frames buffered between the engine thread and the writer. Small on
/// purpose: if the disk cannot keep up, frames are dropped (fail-open)
/// rather than piling up unbounded in memory.
const CHANNEL_CAPACITY: usize = 256;

/// Payload bytes per synthesized RTP packet. Real senders fragment at
/// the MTU; matching that keeps the sidecar's UDP replay well under
/// datagram limits even for large keyframes.
const PAYLOAD_MTU: usize = 1200;

/// The sink handed to the engine. Cheap to clone-reference (`Arc`);
/// everything expensive lives on the writer thread.
pub struct Recorder {
    tx: SyncSender<RecordedFrame>,
}

impl RecorderSink for Recorder {
    fn record(&self, frame: RecordedFrame) {
        // Bounded + non-blocking: recording trouble costs frames,
        // never the meeting.
        match self.tx.try_send(frame) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::debug!("recorder writer is behind; dropping a frame");
            }
            Err(TrySendError::Disconnected(_)) => {
                // Finalized already (stopped, or the meeting emptied).
            }
        }
    }
}

/// Start recording `meeting_id` under `data_dir/recordings/{meeting_id}/`
/// called from async context: the writer thread borrows the current
/// tokio handle for its final metadata UPDATE. Returns the sink to
/// install on the room via the engine.
pub fn start(data_dir: &Path, meeting_id: Uuid, recording_id: Uuid, pool: SqlitePool) -> Recorder {
    // One directory per recording, under the meeting's: re-recording a
    // meeting must not clobber earlier takes.
    let dir = data_dir
        .join("recordings")
        .join(meeting_id.to_string())
        .join(recording_id.to_string());
    // Best-effort: the writer logs per-file failures and degrades.
    if let Err(e) = fs::create_dir_all(&dir) {
        tracing::warn!("could not create {}: {e}", dir.display());
    }
    let handle = Handle::current();
    let (tx, rx) = sync_channel(CHANNEL_CAPACITY);
    match thread::Builder::new()
        .name("huddle-recorder-writer".into())
        .spawn(move || Writer::new(dir, recording_id, handle, pool, rx).run())
    {
        Ok(_join) => {}
        Err(e) => {
            // Fail-open even here: the returned sink simply has no
            // reader, so every frame is dropped on the floor.
            tracing::error!("could not spawn recorder writer: {e}");
        }
    }
    Recorder { tx }
}

/// Which on-disk track a frame belongs to: one file per participant per
/// source, exactly the tiles+audio the sidecar muxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TrackName {
    Audio,
    Video,
    Screen,
}

impl TrackName {
    fn from_frame(frame: &RecordedFrame) -> Self {
        if frame.kind.is_audio() {
            Self::Audio
        } else if frame.screen {
            Self::Screen
        } else {
            Self::Video
        }
    }

    fn file_stem(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Screen => "screen",
        }
    }

    /// The SDP media line for the sidecar manifest. The SFU negotiates
    /// Opus and VP8 only (see `peer.rs`), so the rtpmaps are fixed.
    fn sdp_media(self, pt: u8, port: u16) -> String {
        let (kind, rtpmap) = match self {
            Self::Audio => ("audio", format!("{pt} opus/48000/2")),
            Self::Video | Self::Screen => ("video", format!("{pt} VP8/90000")),
        };
        format!("m={kind} {port} RTP/AVP {pt}\r\nc=IN IP4 127.0.0.1\r\na=rtpmap:{rtpmap}\r\n")
    }
}

/// Identifies one output file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TrackKey {
    origin: Uuid,
    name: TrackName,
}

impl TrackKey {
    /// A stable synthetic SSRC, one per track, so the sidecar's SDP
    /// demuxer can tell streams apart in the replayed datagrams.
    fn ssrc(&self) -> u32 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        h.finish() as u32
    }
}

/// Owns every open file for one recording. `None` entries are tracks
/// whose file could not be created (disk full and friends): they are
/// logged once and then silently skipped — the rest keep recording.
struct Writer {
    dir: PathBuf,
    recording_id: Uuid,
    handle: Handle,
    pool: SqlitePool,
    /// Frames from the engine thread; closes on the last sink drop.
    rx: Receiver<RecordedFrame>,
    files: HashMap<TrackKey, Option<BufWriter<File>>>,
    /// Tracks in first-seen order, with their payload type, for the
    /// manifest.
    tracks: Vec<(TrackKey, u8)>,
    bytes: u64,
}

impl Writer {
    fn new(
        dir: PathBuf,
        recording_id: Uuid,
        handle: Handle,
        pool: SqlitePool,
        rx: Receiver<RecordedFrame>,
    ) -> Self {
        Self {
            dir,
            recording_id,
            handle,
            pool,
            files: HashMap::new(),
            tracks: Vec::new(),
            bytes: 0,
            rx,
        }
    }

    fn run(mut self) {
        // The channel closes when the engine drops the last Recorder
        // reference: explicit stop, or the meeting emptying.
        while let Ok(frame) = self.rx.recv() {
            if let Err(e) = self.write_frame(frame) {
                tracing::warn!("recording write failure: {e}");
            }
        }
        if let Err(e) = self.finalize() {
            tracing::warn!("recording finalize failure for {}: {e}", self.recording_id);
        }
    }

    fn write_frame(&mut self, frame: RecordedFrame) -> io::Result<()> {
        if frame.payload.is_empty() {
            return Ok(());
        }
        let name = TrackName::from_frame(&frame);
        let key = TrackKey {
            origin: frame.origin,
            name,
        };
        let writer = match self.writer_for(&key, frame.pt) {
            Some(w) => w,
            None => return Ok(()), // already logged; fail-open
        };
        let ssrc = key.ssrc();
        let timestamp = frame.timestamp as u32;
        let mut seq = frame.seq;
        let total = frame.payload.len();
        // Counted locally: `self` stays borrowed by the writer for the
        // whole frame.
        let mut written: u64 = 0;
        for (i, chunk) in frame.payload.chunks(PAYLOAD_MTU).enumerate() {
            let last = (i + 1) * PAYLOAD_MTU >= total;
            let header = rtp_header(u8::from(last), frame.pt, seq, timestamp, ssrc);
            // Framing: RTP headers carry no length, so each packet
            // record is preceded by its `u16` big-endian total length
            // (max 12 + 1 descriptor + PAYLOAD_MTU — fits comfortably).
            let mut record_len = (header.len() + chunk.len()) as u16;
            if !matches!(name, TrackName::Audio) {
                record_len += 1; // the VP8 descriptor below
            }
            writer.write_all(&record_len.to_be_bytes())?;
            written += 2;
            writer.write_all(&header)?;
            written += header.len() as u64;
            if !matches!(name, TrackName::Audio) {
                // VP8 payload descriptor (RFC 7741): the S bit marks the
                // first packet of the frame; no extension bits set.
                let descriptor = if i == 0 { 0x10 } else { 0x00 };
                writer.write_all(&[descriptor])?;
                written += 1;
            }
            // Opus goes on the wire as-is (RFC 7587).
            writer.write_all(chunk)?;
            written += chunk.len() as u64;
            seq = seq.wrapping_add(1);
        }
        self.bytes += written;
        Ok(())
    }

    /// The (lazily created) writer for one track, or `None` (after a
    /// one-time warning) if its file cannot be written at all.
    fn writer_for(&mut self, key: &TrackKey, pt: u8) -> Option<&mut BufWriter<File>> {
        if !self.files.contains_key(key) {
            let path = self
                .dir
                .join(format!("{}-{}.rtp", key.origin, key.name.file_stem()));
            match File::create(&path) {
                Ok(file) => {
                    self.files.insert(key.clone(), Some(BufWriter::new(file)));
                    self.tracks.push((key.clone(), pt));
                }
                Err(e) => {
                    tracing::warn!("recording cannot open {}: {e}", path.display());
                    self.files.insert(key.clone(), None);
                }
            }
        }
        self.files.get_mut(key).and_then(|w| w.as_mut())
    }

    /// Flush every file, lay down the sidecar manifest, and finalize
    /// the metadata row from the tokio runtime.
    fn finalize(&mut self) -> io::Result<()> {
        let mut manifest = String::new();
        manifest.push_str("v=0\r\n");
        manifest.push_str(&format!(
            "o=- {} 0 IN IP4 127.0.0.1\r\ns=huddle recording\r\nt=0 0\r\n",
            self.recording_id.hyphenated()
        ));
        for (idx, (key, pt)) in self.tracks.iter().enumerate() {
            // Distinct even loopback ports per m-line for the replay.
            let port = 41_000 + (idx as u16) * 2;
            manifest.push_str(&key.name.sdp_media(*pt, port));
        }
        let writers: Vec<&mut Option<BufWriter<File>>> = self.files.values_mut().collect();
        for w in writers.into_iter().flatten() {
            w.flush()?;
        }
        File::create(self.dir.join("session.sdp"))?.write_all(manifest.as_bytes())?;
        File::create(self.dir.join("manifest.json"))?
            .write_all(self.track_manifest().as_bytes())?;

        // Final bookkeeping happens on the async side; the writer only
        // UPDATEs, and only rows that are still open (never INSERTs).
        let pool = self.pool.clone();
        let id = self.recording_id;
        let bytes = self.bytes;
        self.handle
            .spawn(async move { finalize_row(&pool, id, bytes).await });
        Ok(())
    }

    /// The track table the ffmpeg sidecar consumes: one entry per
    /// m-line of `session.sdp`, in the same order, naming the `.rtp`
    /// file, its UDP replay port, and its payload type. Hand-rolled
    /// JSON: every field is a plain string or number.
    fn track_manifest(&self) -> String {
        let mut json = String::from("{\n  \"recording_id\": \"");
        json.push_str(&self.recording_id.to_string());
        json.push_str("\",\n  \"tracks\": [");
        for (idx, (key, pt)) in self.tracks.iter().enumerate() {
            let port = 41_000 + (idx as u16) * 2;
            let kind = if matches!(key.name, TrackName::Audio) {
                "audio"
            } else {
                "video"
            };
            if idx > 0 {
                json.push(',');
            }
            json.push_str(&format!(
                "\n    {{\"file\": \"{}-{}.rtp\", \"kind\": \"{kind}\", \"pt\": {pt}, \
                 \"ssrc\": {}, \"port\": {port}}}",
                key.origin,
                key.name.file_stem(),
                key.ssrc()
            ));
        }
        json.push_str("\n  ]\n}\n");
        json
    }
}

/// A 12-byte fixed RTP header (no CSRCs, no extensions): str0m hands us
/// depayloadized frames, so the sink reconstructs the packetization.
fn rtp_header(marker: u8, pt: u8, seq: u16, timestamp: u32, ssrc: u32) -> [u8; 12] {
    [
        0x80, // V=2, no padding, no extensions, no CSRCs
        (marker << 7) | pt,
        (seq >> 8) as u8,
        seq as u8,
        (timestamp >> 24) as u8,
        (timestamp >> 16) as u8,
        (timestamp >> 8) as u8,
        timestamp as u8,
        (ssrc >> 24) as u8,
        (ssrc >> 16) as u8,
        (ssrc >> 8) as u8,
        ssrc as u8,
    ]
}

// --- metadata rows ------------------------------------------------------

/// One `recordings` row, as the REST layer sees it.
pub struct RecordingRow {
    pub id: Uuid,
    pub meeting_id: Uuid,
    pub started_by: Option<Uuid>,
    pub started_at: OffsetDateTime,
    pub stopped_at: Option<OffsetDateTime>,
    pub size_bytes: Option<i64>,
}

impl RecordingRow {
    pub fn is_active(&self) -> bool {
        self.stopped_at.is_none()
    }

    pub fn dto(&self) -> RecordingDto {
        RecordingDto {
            id: self.id,
            meeting_id: self.meeting_id,
            started_by: self.started_by,
            started_at: self.started_at,
            stopped_at: self.stopped_at,
            size_bytes: self.size_bytes.map(|b| b as u64),
            active: self.is_active(),
        }
    }
}

/// Insert the row a recording will finalize. Called by the start
/// handler BEFORE the engine is asked to record, so a failed start can
/// simply delete it again.
pub async fn insert_row(
    pool: &SqlitePool,
    id: Uuid,
    meeting_id: Uuid,
    started_by: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    let Ok(started_at) = OffsetDateTime::now_utc().format(&Rfc3339) else {
        return Err(sqlx::Error::Decode("unformattable timestamp".into()));
    };
    sqlx::query(
        "INSERT INTO recordings (id, meeting_id, started_by, started_at)
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(id.to_string())
    .bind(meeting_id.to_string())
    .bind(started_by.map(|u| u.to_string()))
    .bind(started_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Roll back a start that never made it to the engine (or that the
/// engine refused). Runs before the reply, so it wins over any late
/// writer UPDATE — which is additionally guarded by `stopped_at IS NULL`.
pub async fn delete_row(pool: &SqlitePool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM recordings WHERE id = ?1")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// The most recent recording of a meeting (its live one, if any).
pub async fn latest_row(
    pool: &SqlitePool,
    meeting_id: Uuid,
) -> Result<Option<RecordingRow>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, meeting_id, started_by, started_at, stopped_at, size_bytes
         FROM recordings WHERE meeting_id = ?1
         ORDER BY started_at DESC, id DESC LIMIT 1",
    )
    .bind(meeting_id.to_string())
    .fetch_optional(pool)
    .await?;
    row.map(|row| row_to_recording(&row)).transpose()
}

/// Poll the metadata row until the writer's finalize has landed (it is
/// an async UPDATE spawned when the sink dropped). Bounded: returns
/// whatever the row says if the writer is slow or gone.
pub async fn await_stopped(
    pool: &SqlitePool,
    meeting_id: Uuid,
    limit: Duration,
) -> Result<Option<RecordingRow>, sqlx::Error> {
    let deadline = std::time::Instant::now() + limit;
    loop {
        let row = latest_row(pool, meeting_id).await?;
        if row.as_ref().is_some_and(|r| r.stopped_at.is_some()) {
            return Ok(row);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(row);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The writer's one and only write: close out the row. Guarded by
/// `stopped_at IS NULL` so it can never race a rollback.
async fn finalize_row(pool: &SqlitePool, id: Uuid, bytes: u64) {
    let Ok(stopped_at) = OffsetDateTime::now_utc().format(&Rfc3339) else {
        tracing::warn!("recording finalize failed: unformattable timestamp");
        return;
    };
    let result = sqlx::query(
        "UPDATE recordings SET stopped_at = ?2, size_bytes = ?3
         WHERE id = ?1 AND stopped_at IS NULL",
    )
    .bind(id.to_string())
    .bind(stopped_at)
    .bind(bytes as i64)
    .execute(pool)
    .await;
    match result {
        Ok(res) if res.rows_affected() == 0 => {
            tracing::debug!("recording {id} already finalized (or rolled back)");
        }
        Ok(_) => tracing::info!("recording {id} finalized ({bytes} bytes)"),
        Err(e) => tracing::warn!("recording finalize failed: {e}"),
    }
}

fn row_to_recording(row: &sqlx::sqlite::SqliteRow) -> Result<RecordingRow, sqlx::Error> {
    let id = Uuid::parse_str(row.try_get::<String, _>("id")?.as_str())
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let meeting_id = Uuid::parse_str(row.try_get::<String, _>("meeting_id")?.as_str())
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let started_by = row
        .try_get::<Option<String>, _>("started_by")?
        .map(|raw| Uuid::parse_str(&raw))
        .transpose()
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let started_at =
        OffsetDateTime::parse(row.try_get::<String, _>("started_at")?.as_str(), &Rfc3339)
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    let stopped_at = row
        .try_get::<Option<String>, _>("stopped_at")?
        .map(|raw| OffsetDateTime::parse(&raw, &Rfc3339))
        .transpose()
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    Ok(RecordingRow {
        id,
        meeting_id,
        started_by,
        started_at,
        stopped_at,
        size_bytes: row.try_get("size_bytes")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The sink drops frames without a reader instead of failing —
    /// that is the whole fail-open contract, observable here as a
    /// channel that accepts-and-discards until it fills, then drops.
    #[tokio::test]
    async fn recorder_without_writer_never_blocks() {
        let pool = sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let recorder = start(
            Path::new("/nonexistent-huddle-test"),
            Uuid::new_v4(),
            Uuid::new_v4(),
            pool,
        );
        // No writer thread will ever drain this: every send after the
        // capacity is silently dropped, and `record` returns promptly.
        for i in 0..CHANNEL_CAPACITY * 4 {
            let frame = RecordedFrame {
                origin: Uuid::new_v4(),
                kind: str0m::media::MediaKind::Audio,
                screen: false,
                pt: 111,
                seq: (i % u16::MAX as usize) as u16,
                timestamp: i as u64 * 960,
                contiguous: true,
                keyframe: false,
                payload: Arc::from(vec![0u8; 40].into_boxed_slice()),
            };
            recorder.record(frame);
        }
        // Dropping the sink closes the channel; give the writer a beat
        // to finalize (its DB update harmlessly fails on this pool).
        drop(recorder);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
