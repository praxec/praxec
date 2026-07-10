//! Tests for `FileAuditSink` date rotation and category splitting.
//!
//! All tests use `InMemoryFilesystem` — no `TempDir`, no real I/O. Each test
//! creates its own filesystem instance so they are fully independent and
//! parallel-safe by construction.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use praxec_core::audit::{
    AuditEvent, AuditRetention, AuditSink, FileAuditSink, RotationInterval, parse_filename_stamp,
};
use praxec_core::fs::{Filesystem as _, InMemoryFilesystem};

fn nz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).expect("nonzero")
}

/// Build a deterministic `FileAuditSink` whose clock is backed by a shared
/// `Arc<Mutex<DateTime<Utc>>>` and whose I/O is entirely in-memory.
fn make_sink(
    dir: impl Into<PathBuf>,
    interval: RotationInterval,
    initial: DateTime<Utc>,
) -> (FileAuditSink, Arc<Mutex<DateTime<Utc>>>, InMemoryFilesystem) {
    let clock_state = Arc::new(Mutex::new(initial));
    let clock_for_sink = clock_state.clone();
    let mem_fs = InMemoryFilesystem::new();
    let sink = FileAuditSink::with_clock_and_fs(
        dir,
        interval,
        Box::new(move || *clock_for_sink.lock().unwrap()),
        Arc::new(mem_fs.clone()),
    );
    (sink, clock_state, mem_fs)
}

// ---------------------------------------------------------------------------
// Test 1 — rotation: two events on different dates land in different files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn file_sink_rotates_on_interval() {
    let dir = PathBuf::from("/audit");

    // Pin clock to 2026-01-15 12:00 UTC (daily rotation)
    let t1: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
    let (sink, clock, mem_fs) = make_sink(dir, RotationInterval::Daily, t1);

    // Record first event at 2026-01-15
    let event1 = AuditEvent::new("workflow.started");
    sink.record(event1).await.expect("first record");

    // Advance clock to the next day
    let t2: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 1, 16, 0, 5, 0).unwrap();
    *clock.lock().unwrap() = t2;

    // Record second event at 2026-01-16
    let event2 = AuditEvent::new("workflow.started");
    sink.record(event2).await.expect("second record");

    // Collect all -audit.log files from the in-memory filesystem.
    let mut files: Vec<String> = mem_fs
        .files()
        .into_iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
        .filter(|n| n.ends_with("-audit.log"))
        .collect();
    files.sort();

    assert_eq!(
        files.len(),
        2,
        "expected two dated audit log files, got: {:?}",
        files
    );
    assert!(
        files[0].contains("2026-01-15"),
        "first file should be for 2026-01-15, got: {}",
        files[0]
    );
    assert!(
        files[1].contains("2026-01-16"),
        "second file should be for 2026-01-16, got: {}",
        files[1]
    );
}

// ---------------------------------------------------------------------------
// Test 2 — category split: transitions go to -transitions.log; rest to -audit.log
// ---------------------------------------------------------------------------

#[tokio::test]
async fn transition_and_audit_streams_split_by_name() {
    let dir = PathBuf::from("/audit");
    let fixed: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 3, 10, 9, 0, 0).unwrap();
    let (sink, _clock, mem_fs) = make_sink(dir, RotationInterval::Daily, fixed);
    let stamp = "2026-03-10";

    // Record a workflow.transition event (goes to transitions log)
    sink.record(AuditEvent::new("workflow.transition"))
        .await
        .expect("transition record");

    // Record an unrelated event (goes to audit log)
    sink.record(AuditEvent::new("workflow.started"))
        .await
        .expect("audit record");

    let pid = std::process::id();
    let transitions_path = PathBuf::from(format!("/audit/{stamp}-{pid}-transitions.log"));
    let audit_path = PathBuf::from(format!("/audit/{stamp}-{pid}-audit.log"));

    let files = mem_fs.files();
    let paths: Vec<&PathBuf> = files.iter().map(|(p, _)| p).collect();

    assert!(
        paths.contains(&&transitions_path),
        "transitions log should exist, got paths: {:?}",
        paths
    );
    assert!(
        paths.contains(&&audit_path),
        "audit log should exist, got paths: {:?}",
        paths
    );

    // Verify content: transitions log has exactly one line
    let trans_content = files
        .iter()
        .find(|(p, _)| p == &transitions_path)
        .map(|(_, c)| c.clone())
        .unwrap();
    let trans_lines: Vec<&str> = trans_content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        trans_lines.len(),
        1,
        "transitions log should have exactly one event"
    );
    let trans_event: serde_json::Value = serde_json::from_str(trans_lines[0]).unwrap();
    assert_eq!(
        trans_event["event_type"], "workflow.transition",
        "transitions log should contain the transition event"
    );

    // Verify content: audit log has exactly one line
    let audit_content = files
        .iter()
        .find(|(p, _)| p == &audit_path)
        .map(|(_, c)| c.clone())
        .unwrap();
    let audit_lines: Vec<&str> = audit_content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        audit_lines.len(),
        1,
        "audit log should have exactly one event"
    );
    let audit_parsed: serde_json::Value = serde_json::from_str(audit_lines[0]).unwrap();
    assert_eq!(
        audit_parsed["event_type"], "workflow.started",
        "audit log should contain the non-transition event"
    );
}

// ---------------------------------------------------------------------------
// Test 2b — heartbeat category split: agent.heartbeat gets its OWN per-writer
// file, and a governance read (try_list_events) EXCLUDES it.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn heartbeat_lands_in_its_own_file_excluded_from_governance_reads() {
    let dir = PathBuf::from("/audit");
    let fixed: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 4, 2, 9, 0, 0).unwrap();
    let (sink, _clock, mem_fs) = make_sink(dir, RotationInterval::Daily, fixed);
    let pid = std::process::id();

    // A governance event + a high-frequency heartbeat pulse.
    sink.record(AuditEvent::new("workflow.started"))
        .await
        .expect("audit record");
    sink.record(AuditEvent::new("agent.heartbeat"))
        .await
        .expect("heartbeat record");

    // (d) — the per-writer filenames carry the pid AND the heartbeat is a
    // distinct category file.
    let names: Vec<String> = mem_fs
        .files()
        .into_iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.contains(&format!("2026-04-02-{pid}-audit.log")),
        "audit stream should be a per-writer pid file, got: {names:?}"
    );
    assert!(
        names.contains(&format!("2026-04-02-{pid}-heartbeat.log")),
        "heartbeat should route to its OWN per-writer pid file, got: {names:?}"
    );

    // (c) — a governance read merges the audit stream but NOT the heartbeat.
    let events = sink
        .try_list_events()
        .await
        .expect("read ok")
        .expect("some events");
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert!(
        types.contains(&"workflow.started"),
        "governance read includes the audit stream, got: {types:?}"
    );
    assert!(
        !types.contains(&"agent.heartbeat"),
        "governance read must EXCLUDE the heartbeat pulse stream, got: {types:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — hourly rotation stamp format
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hourly_rotation_uses_hour_stamp() {
    let dir = PathBuf::from("/audit");
    let t1: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 6, 1, 14, 30, 0).unwrap();
    let (sink, clock, mem_fs) = make_sink(dir, RotationInterval::Hourly, t1);

    sink.record(AuditEvent::new("workflow.started"))
        .await
        .unwrap();

    // Advance past the hour boundary
    let t2: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 6, 1, 15, 2, 0).unwrap();
    *clock.lock().unwrap() = t2;

    sink.record(AuditEvent::new("workflow.started"))
        .await
        .unwrap();

    let mut files: Vec<String> = mem_fs
        .files()
        .into_iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
        .filter(|n| n.ends_with("-audit.log"))
        .collect();
    files.sort();

    assert_eq!(
        files.len(),
        2,
        "hourly rotation should produce two files, got: {:?}",
        files
    );
    assert!(
        files[0].contains("2026-06-01-14"),
        "first file should contain hour 14, got: {}",
        files[0]
    );
    assert!(
        files[1].contains("2026-06-01-15"),
        "second file should contain hour 15, got: {}",
        files[1]
    );
}

// ---------------------------------------------------------------------------
// Test 4 — weekly rotation stamp format
// ---------------------------------------------------------------------------

#[tokio::test]
async fn weekly_rotation_uses_iso_week_stamp() {
    let dir = PathBuf::from("/audit");
    let t1: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 1, 12, 10, 0, 0).unwrap();
    let (sink, clock, mem_fs) = make_sink(dir, RotationInterval::Weekly, t1);

    sink.record(AuditEvent::new("workflow.started"))
        .await
        .unwrap();

    // Advance to a date in a different ISO week (more than 10 days apart)
    let t2: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 1, 26, 10, 0, 0).unwrap();
    *clock.lock().unwrap() = t2;

    sink.record(AuditEvent::new("workflow.started"))
        .await
        .unwrap();

    let mut files: Vec<String> = mem_fs
        .files()
        .into_iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
        .filter(|n| n.ends_with("-audit.log"))
        .collect();
    files.sort();

    assert_eq!(
        files.len(),
        2,
        "weekly rotation should produce two files, got: {:?}",
        files
    );
    assert!(
        files[0].contains("2026-W03"),
        "first file should contain ISO week 2026-W03, got: {}",
        files[0]
    );
    assert!(
        files[1].contains("2026-W05"),
        "second file should contain ISO week 2026-W05, got: {}",
        files[1]
    );
}

// ---------------------------------------------------------------------------
// Test 5 — sub-daily minutes granule: stamp floors to the granule boundary
// with full YYYY-MM-DD-HH-MM-SS precision; crossing the boundary rotates.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn minutes_granule_rotates_on_floored_boundary() {
    let dir = PathBuf::from("/audit");
    // 12:07 with a 5-minute granule → boundary 12:05.
    let t1: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 6, 1, 12, 7, 30).unwrap();
    let (sink, clock, mem_fs) = make_sink(dir, RotationInterval::Minutes(nz(5)), t1);

    sink.record(AuditEvent::new("workflow.started"))
        .await
        .unwrap();

    // 12:09 → same 12:05 window (no rotation yet).
    *clock.lock().unwrap() = Utc.with_ymd_and_hms(2026, 6, 1, 12, 9, 59).unwrap();
    sink.record(AuditEvent::new("workflow.started"))
        .await
        .unwrap();

    // 12:10 → next window.
    *clock.lock().unwrap() = Utc.with_ymd_and_hms(2026, 6, 1, 12, 10, 0).unwrap();
    sink.record(AuditEvent::new("workflow.started"))
        .await
        .unwrap();

    let mut files: Vec<String> = mem_fs
        .files()
        .into_iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
        .filter(|n| n.ends_with("-audit.log"))
        .collect();
    files.sort();

    assert_eq!(
        files.len(),
        2,
        "5-minute granule: 12:07 + 12:09 share a file, 12:10 rotates; got: {files:?}"
    );
    assert!(
        files[0].starts_with("2026-06-01-12-05-00-"),
        "first window floors to 12:05 with SS precision, got: {}",
        files[0]
    );
    assert!(
        files[1].starts_with("2026-06-01-12-10-00-"),
        "second window floors to 12:10, got: {}",
        files[1]
    );
}

// ---------------------------------------------------------------------------
// Retention — filename timestamp IS the retention key
// ---------------------------------------------------------------------------

/// Expired rotated files (by filename timestamp) are deleted on the next
/// rotation boundary; in-window files and unparseable names survive.
#[tokio::test]
async fn retention_deletes_expired_files_by_filename_timestamp() {
    let dir = PathBuf::from("/audit");
    let now: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
    let clock_state = Arc::new(Mutex::new(now));
    let clock_for_sink = clock_state.clone();
    let mem_fs = InMemoryFilesystem::new();

    // Pre-seed: one file two days old (expired for a 24h window), one from
    // this morning (in-window), one unparseable (must NEVER be deleted).
    mem_fs.create_dir_all(&dir).await.unwrap();
    mem_fs
        .append(&dir.join("2026-06-01-99-audit.log"), b"{}\n")
        .await
        .unwrap();
    mem_fs
        .append(&dir.join("2026-06-03-08-99-audit.log"), b"{}\n")
        .await
        .unwrap();
    mem_fs
        .append(&dir.join("not-a-stamp-audit.log"), b"{}\n")
        .await
        .unwrap();

    let sink = FileAuditSink::with_clock_and_fs(
        dir,
        RotationInterval::Hourly,
        Box::new(move || *clock_for_sink.lock().unwrap()),
        Arc::new(mem_fs.clone()),
    )
    .with_retention(AuditRetention {
        keep_for_hours: nz(24),
    });

    // First write on the 12:00 stamp triggers the sweep.
    sink.record(AuditEvent::new("workflow.started"))
        .await
        .unwrap();

    let names: Vec<String> = mem_fs
        .files()
        .into_iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.contains(&"2026-06-01-99-audit.log".to_string()),
        "the two-day-old file is outside the 24h window and must be deleted; got: {names:?}"
    );
    assert!(
        names.contains(&"2026-06-03-08-99-audit.log".to_string()),
        "this morning's file is inside the window and must survive; got: {names:?}"
    );
    assert!(
        names.contains(&"not-a-stamp-audit.log".to_string()),
        "a file whose name doesn't parse must NEVER be deleted (fail-safe); got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.starts_with("2026-06-03-12-")),
        "the new write landed in the current-stamp file; got: {names:?}"
    );
}

/// The currently-open stamp is exempt even when its PARSED timestamp falls
/// outside the window — a daily stamp parses as midnight, which a short
/// `keep_for_hours: 1` window would otherwise consider expired late in the
/// day. Deleting the open write target would drop live governance events.
#[tokio::test]
async fn retention_never_deletes_the_current_stamp_file() {
    let dir = PathBuf::from("/audit");
    // 23:00 — today's daily stamp parses as 00:00, 23h ago (> the 1h window).
    let t1: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 6, 3, 23, 0, 0).unwrap();
    let clock_state = Arc::new(Mutex::new(t1));
    let clock_for_sink = clock_state.clone();
    let mem_fs = InMemoryFilesystem::new();
    let sink = FileAuditSink::with_clock_and_fs(
        dir,
        RotationInterval::Daily,
        Box::new(move || *clock_for_sink.lock().unwrap()),
        Arc::new(mem_fs.clone()),
    )
    .with_retention(AuditRetention {
        keep_for_hours: nz(1),
    });

    sink.record(AuditEvent::new("workflow.started"))
        .await
        .unwrap();
    // Second write on the SAME stamp (sweep already ran for this boundary,
    // but even a fresh sweep must exempt the current stamp).
    *clock_state.lock().unwrap() = Utc.with_ymd_and_hms(2026, 6, 3, 23, 30, 0).unwrap();
    sink.record(AuditEvent::new("workflow.completed"))
        .await
        .unwrap();

    let events = sink
        .try_list_events()
        .await
        .expect("read ok")
        .expect("events survive");
    assert_eq!(
        events.len(),
        2,
        "today's file holds both events — never deleted while current"
    );
}

/// Without `with_retention` (the default) nothing is ever deleted, no matter
/// how old the files are — retention is strictly opt-in.
#[tokio::test]
async fn no_retention_configured_deletes_nothing() {
    let dir = PathBuf::from("/audit");
    let now: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
    let (sink, _clock, mem_fs) = make_sink(dir.clone(), RotationInterval::Daily, now);

    mem_fs.create_dir_all(&dir).await.unwrap();
    mem_fs
        .append(&dir.join("2020-01-01-99-audit.log"), b"{}\n")
        .await
        .unwrap();

    sink.record(AuditEvent::new("workflow.started"))
        .await
        .unwrap();

    let names: Vec<String> = mem_fs
        .files()
        .into_iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.contains(&"2020-01-01-99-audit.log".to_string()),
        "no retention configured → even a 6-year-old file survives; got: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// parse_filename_stamp — the naming convention IS the retention key
// ---------------------------------------------------------------------------

#[test]
fn filename_stamp_parses_every_rotation_format() {
    // Daily (with pid).
    assert_eq!(
        parse_filename_stamp("2026-07-10-12345-audit.log"),
        Some(Utc.with_ymd_and_hms(2026, 7, 10, 0, 0, 0).unwrap())
    );
    // Legacy daily (no pid segment).
    assert_eq!(
        parse_filename_stamp("2026-07-10-audit.log"),
        Some(Utc.with_ymd_and_hms(2026, 7, 10, 0, 0, 0).unwrap())
    );
    // Hourly.
    assert_eq!(
        parse_filename_stamp("2026-07-10-14-12345-transitions.log"),
        Some(Utc.with_ymd_and_hms(2026, 7, 10, 14, 0, 0).unwrap())
    );
    // Minutes granule (full seconds precision).
    assert_eq!(
        parse_filename_stamp("2026-07-10-12-05-00-12345-heartbeat.log"),
        Some(Utc.with_ymd_and_hms(2026, 7, 10, 12, 5, 0).unwrap())
    );
    // Weekly — Monday of ISO week 28, 2026 = 2026-07-06.
    assert_eq!(
        parse_filename_stamp("2026-W28-12345-audit.log"),
        Some(Utc.with_ymd_and_hms(2026, 7, 6, 0, 0, 0).unwrap())
    );
    // Unparseable → None (retention never deletes what it cannot date).
    assert_eq!(parse_filename_stamp("not-a-stamp-audit.log"), None);
    assert_eq!(parse_filename_stamp("audit.log"), None);
}

/// The one documented pid/stamp ambiguity resolves in the SAFE direction:
/// a daily file whose pid looks like a valid hour parses as a LATER instant
/// (retained longer), never an earlier one (premature deletion impossible).
#[test]
fn filename_stamp_ambiguity_errs_toward_retaining_longer() {
    let daily_midnight = Utc.with_ymd_and_hms(2026, 7, 10, 0, 0, 0).unwrap();
    // pid `12` is indistinguishable from hour 12 → parses as 12:00 (later).
    let parsed = parse_filename_stamp("2026-07-10-12-audit.log").expect("parses");
    assert!(
        parsed >= daily_midnight,
        "ambiguous parse must never predate the true window start"
    );
    // pid `99` is NOT a valid hour → falls through to the true daily parse.
    assert_eq!(
        parse_filename_stamp("2026-07-10-99-audit.log"),
        Some(daily_midnight)
    );
}
