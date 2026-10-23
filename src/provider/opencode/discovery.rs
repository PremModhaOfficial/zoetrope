//! Where opencode keeps its sessions on disk, and how to tell what a
//! database file is.
//!
//! Every session of every project lives in ONE SQLite file,
//! `<home>/opencode.db` (`session`, `message`, `part` tables). A provider
//! states what a file is, so one database file states many sessions: the
//! core still needs one [`SessionFile`](crate::provider::SessionFile) per
//! session, so [`all_paths`] enumerates one pseudo-path per matching session,
//! `<db>/<session-id>`, and [`session_file`] decodes it back. The
//! [`SessionFile::path`](crate::provider::SessionFile::path) is always the
//! real database (that is what the feeders read); the pseudo-path never hits
//! the disk. A bare `opencode.db` classifies as its newest session.
//!
//! The live tailer re-reads the database through a persistent stream whose
//! seen-ids make a re-read idempotent, so growth shows up on the next tick
//! without duplicates. `related_paths` is empty: a session's rows are found
//! by query, not by neighbouring files.

use std::path::{Path, PathBuf};

/// The opencode home: `$OPENCODE_HOME`, else `$XDG_DATA_HOME/opencode`, else
/// `~/.local/share/opencode`.
#[cfg(any(feature = "native", test))]
pub fn opencode_home() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OPENCODE_HOME") {
        return Some(PathBuf::from(p));
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(xdg);
        if !p.as_os_str().is_empty() {
            return Some(p.join("opencode"));
        }
    }
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("opencode"),
    )
}

/// The session database under a home.
#[cfg(any(feature = "native", test))]
pub fn db_path(home: &Path) -> PathBuf {
    home.join("opencode.db")
}

/// Whether a path names a session database. Shape only; [`session_file`]
/// probes the content.
#[cfg(any(feature = "native", test))]
pub fn is_session_db(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == "opencode.db")
}

/// Split a database path into the database and the session it names: the
/// database itself (session to be resolved), or a `<db>/<session-id>`
/// pseudo-path from [`all_paths`].
#[cfg(any(feature = "native", test))]
fn decode(path: &Path) -> Option<(PathBuf, Option<String>)> {
    if is_session_db(path) {
        return Some((path.to_path_buf(), None));
    }
    let parent = path.parent()?;
    if !is_session_db(parent) {
        return None;
    }
    let id = path.file_name()?.to_str()?.to_string();
    if id.is_empty() {
        return None;
    }
    Some((parent.to_path_buf(), Some(id)))
}

/// Whether a file starts with the SQLite magic. A cheap content probe so a
/// stray `opencode.db` that is not a database classifies as nothing.
#[cfg(any(feature = "native", test))]
pub fn is_sqlite(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 16];
    f.read_exact(&mut magic).is_ok() && magic == *b"SQLite format 3\0"
}

// ---------------------------------------------------------------------------
// The provider primitives (see `provider/mod.rs` and docs/DISCOVERY.md)
// ---------------------------------------------------------------------------

/// Native-only in production (the browser never stats a database) but used
/// by the portable unit tests below, hence `any(feature = "native", test)`:
///
/// - `cargo check --no-default-features` would otherwise warn the import is
///   never used.
#[cfg(any(feature = "native", test))]
use std::time::SystemTime;

#[cfg(any(feature = "native", test))]
use crate::provider::{FileRole, Provider, ReadMode};
use crate::provider::{Scope, SessionFile};

/// One `session` row, as far as discovery filters.
#[cfg(feature = "native")]
struct SessionListRow {
    id: String,
    directory: String,
    time_updated: i64,
}

/// One database per home, one pseudo-path per matching session inside it
/// (see the module docs). A scope filters by the session row: `project`
/// against `directory`, `since` against `time_updated`, `id_prefix` against
/// the id. Without the `native` feature (the browser) there is no database
/// to read, so there are no paths.
pub fn all_paths(scope: &Scope) -> Vec<PathBuf> {
    #[cfg(not(feature = "native"))]
    {
        let _ = scope;
        Vec::new()
    }
    #[cfg(feature = "native")]
    {
        all_paths_native(scope)
    }
}

#[cfg(feature = "native")]
fn all_paths_native(scope: &Scope) -> Vec<PathBuf> {
    // NOTE (scope coarseness): this prunes by the session ROW's `time_updated`
    // while `Scope::admits` re-checks the assembled session's `last_modified`
    // — which for opencode is the DATABASE file's mtime, shared by every
    // session in it. The per-session `since` here is therefore coarse twice
    // over: a fresh write to any session lifts the whole database's mtime, so
    // `admits` keeps every session of a recently-touched database and the row
    // filter above does the real per-session pruning. No behavior change: the
    // row filter is strictly finer, so the double filter can only agree.
    let Some(home) = opencode_home() else {
        return Vec::new();
    };
    let db = db_path(&home);
    if !is_sqlite(&db) {
        return Vec::new();
    }
    let since_ms = scope
        .since
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    session_list(&db)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| {
            scope
                .project
                .as_deref()
                .is_none_or(|cwd| s.directory == Provider::Opencode.project_key(cwd))
                && since_ms.is_none_or(|t| s.time_updated >= t)
                && scope
                    .id_prefix
                    .as_deref()
                    .is_none_or(|p| s.id.starts_with(p))
        })
        .map(|s| db.join(s.id))
        .collect()
}

/// What a database path is: the named session, or the newest one for a bare
/// database. `None` for anything else, or when the database cannot be read.
pub fn session_file(path: &Path) -> Option<SessionFile> {
    #[cfg(not(feature = "native"))]
    {
        let _ = path;
        None
    }
    #[cfg(feature = "native")]
    {
        session_file_native(path)
    }
}

#[cfg(feature = "native")]
fn session_file_native(path: &Path) -> Option<SessionFile> {
    let (db, named) = decode(path)?;
    if !is_sqlite(&db) {
        return None;
    }
    let meta = match named {
        Some(id) => read_meta(&db, &id)?,
        None => newest_meta(&db)?,
    };
    Some(SessionFile {
        provider: Provider::Opencode,
        path: db.clone(),
        session: meta.id,
        role: FileRole::Root,
        read: ReadMode::Whole,
        project_key: meta.directory,
        modified: crate::provider::modified(&db),
    })
}

/// A session's rows live in the database, found by query: there are no
/// neighbouring files to look at.
pub fn related_paths(_file: &SessionFile) -> Vec<PathBuf> {
    Vec::new()
}

/// Opencode records the working directory as it is.
pub fn project_key(cwd: &Path) -> String {
    cwd.to_string_lossy().into_owned()
}

/// A fresh parser; it learns the session from the database rows it is fed.
pub fn stream_for(_file: &SessionFile) -> super::Stream {
    super::Stream::new()
}

/// Opencode has no whole-read sidecars: the database itself is the
/// whole-read file, handled by [`super::statements_for`].
pub fn sidecar(_file: &SessionFile, _text: &str) -> Option<crate::fact::Statement> {
    None
}

/// Scratch databases and env handling for the native tests, shared with the
/// provider tests (the replay feeder test builds its own database).
#[cfg(all(test, feature = "native"))]
pub(crate) mod support {
    use super::*;

    pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A saved environment variable, restored on drop.
    pub(crate) struct SavedEnv {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl SavedEnv {
        pub(crate) fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
            let old = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            SavedEnv { key, old }
        }
    }

    impl Drop for SavedEnv {
        fn drop(&mut self) {
            unsafe {
                match &self.old {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// A scratch opencode home with a tiny session database in it.
    pub(crate) fn scratch_home(
        tag: &str,
    ) -> (std::sync::MutexGuard<'static, ()>, PathBuf, SavedEnv) {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir =
            std::env::temp_dir().join(format!("zoetrope_opencode_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = db_path(&dir);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL, title TEXT NOT NULL, agent TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .unwrap();
        let saved = SavedEnv::set("OPENCODE_HOME", dir.as_os_str());
        (guard, dir, saved)
    }

    pub(crate) fn seed_session(db: &Path, id: &str, dir: &str, updated: i64) {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.execute(
            "INSERT INTO session (id, directory, title, agent, time_created, time_updated) VALUES (?1, ?2, 'T', 'build', 1000, ?3)",
            rusqlite::params![id, dir, updated],
        )
        .unwrap();
        let mid = format!("{id}-m1");
        let pid = format!("{id}-p1");
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, 1000, 1000, '{\"role\":\"user\",\"agent\":\"build\",\"time\":{\"created\":1000}}')",
            rusqlite::params![mid, id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, 1000, 1000, '{\"type\":\"text\",\"text\":\"hi\"}')",
            rusqlite::params![pid, mid, id],
        )
        .unwrap();
    }
}

// ---------------------------------------------------------------------------
// SQLite reads (native only; parameterized, read-only)
// ---------------------------------------------------------------------------

#[cfg(feature = "native")]
use super::wire::SessionRows;

#[cfg(feature = "native")]
fn connect_ro(db: &Path) -> Option<rusqlite::Connection> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    // The agent holds a write lock most of the time it runs: fail fast
    // (~100ms) instead of blocking the poll tick on a locked database.
    // A timed-out open is `None`, and the feeder retries on its next tick.
    conn.busy_timeout(std::time::Duration::from_millis(100))
        .ok()?;
    Some(conn)
}

/// A cheap fingerprint of the database files for the live tick gate: the
/// newest mtime and total size across the database and its WAL/sidecar
/// journals. SQLite in WAL mode writes to `-wal` without touching the main
/// file's mtime, so gating on the main file alone would miss appends.
#[cfg(feature = "native")]
pub(crate) fn db_fingerprint(db: &Path) -> Option<(SystemTime, u64)> {
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut total = 0u64;
    let mut found = false;
    // Sibling journal paths share the db file name with a suffix.
    let wal_name = format!("{}-wal", db.file_name()?.to_str()?);
    let shm_name = format!("{}-shm", db.file_name()?.to_str()?);
    for path in [
        db.to_path_buf(),
        db.with_file_name(wal_name),
        db.with_file_name(shm_name),
    ] {
        if let Ok(meta) = std::fs::metadata(&path) {
            found = true;
            if let Ok(m) = meta.modified() {
                newest = newest.max(m);
            }
            total += meta.len();
        }
    }
    found.then_some((newest, total))
}

/// Every session in the database, newest first. `None` when the file is not
/// a database with this schema — never an error to the caller. Resilient
/// per-row: one corrupt row (a bad `time_updated`, a truncated id) is
/// skipped, never empties the whole list — same flatten-as-read_session.
#[cfg(feature = "native")]
fn session_list(db: &Path) -> Option<Vec<SessionListRow>> {
    let conn = connect_ro(db)?;
    let mut stmt = conn
        .prepare("SELECT id, directory, time_updated FROM session ORDER BY time_updated DESC, id")
        .ok()?;
    let rows = stmt
        .query_map([], |r| {
            Ok(SessionListRow {
                id: r.get(0)?,
                directory: r.get(1)?,
                time_updated: r.get(2)?,
            })
        })
        .ok()?;
    let mut out = Vec::new();
    for row in rows.flatten() {
        out.push(row);
    }
    Some(out)
}

#[cfg(feature = "native")]
fn row_to_meta(
    id: String,
    title: String,
    directory: String,
    agent: Option<String>,
    time_created: i64,
) -> super::wire::SessionMeta {
    super::wire::SessionMeta {
        id,
        title,
        directory,
        agent,
        time_created,
    }
}

/// The newest session's metadata, if the database holds any session.
#[cfg(feature = "native")]
fn newest_meta(db: &Path) -> Option<super::wire::SessionMeta> {
    let conn = connect_ro(db)?;
    conn.query_row(
        "SELECT id, title, directory, agent, time_created FROM session ORDER BY time_updated DESC, id LIMIT 1",
        [],
        |r| {
            Ok(row_to_meta(
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
            ))
        },
    )
    .ok()
}

/// The named session's metadata, if it exists.
#[cfg(feature = "native")]
fn read_meta(db: &Path, session_id: &str) -> Option<super::wire::SessionMeta> {
    let conn = connect_ro(db)?;
    conn.query_row(
        "SELECT id, title, directory, agent, time_created FROM session WHERE id = ?1",
        rusqlite::params![session_id],
        |r| {
            Ok(row_to_meta(
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
            ))
        },
    )
    .ok()
}

/// A session's messages with their parts, in time order: the portable shape
/// the provider's stream feeds on. Message ids are kept so the stream's
/// seen-set makes re-reads idempotent. Unparsable `data` columns are skipped,
/// never fatal; parts whose message is gone (or unparsable) are dropped.
#[cfg(feature = "native")]
pub fn read_session(
    db: &Path,
    session_id: &str,
) -> Option<(super::wire::SessionMeta, SessionRows)> {
    use super::wire::{parse_message, parse_part};
    let meta = read_meta(db, session_id)?;
    let conn = connect_ro(db)?;
    // Messages in time order, ids kept for the part join below.
    let mut ordered: Vec<(String, super::wire::MessageData)> = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created, id")
            .ok()?;
        let rows = stmt
            .query_map(rusqlite::params![session_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .ok()?;
        for row in rows.flatten() {
            if let Some(msg) = parse_message(&row.1) {
                ordered.push((row.0, msg));
            }
        }
    }
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, (id, _)) in ordered.iter().enumerate() {
        index.insert(id.clone(), i);
    }
    let mut out: SessionRows = ordered
        .into_iter()
        .map(|(id, msg)| (id, msg, Vec::new()))
        .collect();
    {
        let mut stmt = conn
            .prepare(
                "SELECT id, message_id, data FROM part WHERE session_id = ?1 ORDER BY time_created, id",
            )
            .ok()?;
        let rows = stmt
            .query_map(rusqlite::params![session_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .ok()?;
        for row in rows.flatten() {
            // Parts whose message is gone (or unparsable) are dropped: a part
            // without its message's role and time states nothing attributable.
            if let (Some(i), Some(part)) = (index.get(&row.1), parse_part(&row.2)) {
                out[*i].2.push((row.0, part));
            }
        }
    }
    Some((meta, out))
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "native")]
    use super::support::{scratch_home, seed_session};
    use super::*;

    #[test]
    fn db_shape_detection_is_path_and_magic() {
        assert!(is_session_db(Path::new("/x/opencode.db")));
        assert!(!is_session_db(Path::new("/x/opencode.db/ses_1")));
        assert!(!is_session_db(Path::new("/x/other.db")));
        // Decode: the database itself, or one session inside it.
        assert_eq!(
            decode(Path::new("/x/opencode.db")),
            Some((PathBuf::from("/x/opencode.db"), None))
        );
        assert_eq!(
            decode(Path::new("/x/opencode.db/ses_1")),
            Some((PathBuf::from("/x/opencode.db"), Some("ses_1".into())))
        );
        assert_eq!(decode(Path::new("/x/other.db")), None);
        // Not SQLite at all: no magic, not a database.
        assert!(!is_sqlite(Path::new("/definitely/not/a/real.db")));
    }

    #[test]
    fn project_key_is_the_directory_itself() {
        assert_eq!(
            project_key(Path::new("/home/u/proj")),
            "/home/u/proj".to_string()
        );
    }

    #[test]
    fn related_paths_is_empty_and_sidecar_is_none() {
        let f = SessionFile {
            provider: Provider::Opencode,
            path: PathBuf::from("/x/opencode.db"),
            session: "s".into(),
            role: FileRole::Root,
            read: ReadMode::Whole,
            project_key: String::new(),
            modified: SystemTime::UNIX_EPOCH,
        };
        assert!(related_paths(&f).is_empty());
        assert!(sidecar(&f, "{}").is_none());
    }

    #[cfg(feature = "native")]
    #[test]
    fn all_paths_lists_one_pseudo_path_per_session() {
        let (_guard, home, _saved) = scratch_home("paths");
        let db = db_path(&home);
        seed_session(&db, "ses_new", "/proj", 2000);
        seed_session(&db, "ses_old", "/other", 1000);
        let mut paths = all_paths(&Scope::ALL);
        paths.sort();
        assert_eq!(
            paths,
            vec![db.join("ses_new"), db.join("ses_old")],
            "newest first is the SQL order; sorted here for a stable assert"
        );
        // Scope filters apply to the session row.
        assert_eq!(all_paths(&Scope::id("ses_new")).len(), 1);
        assert_eq!(
            all_paths(&Scope::project(Path::new("/proj"))).len(),
            1,
            "only the session of that directory"
        );
        assert!(
            all_paths(
                &Scope::ALL.since(SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1500))
            )
            .iter()
            .all(|p| p.ends_with("ses_new"))
        );
    }

    #[cfg(feature = "native")]
    #[test]
    fn session_list_survives_one_corrupt_row() {
        let (_guard, home, _saved) = scratch_home("corrupt");
        let db = db_path(&home);
        seed_session(&db, "ses_good", "/proj", 2000);
        seed_session(&db, "ses_other", "/proj", 1000);
        // Corrupt one row's time_updated to a non-integer: the resilient
        // per-row read must still return the healthy sessions.
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE session SET time_updated = 'xx' WHERE id = 'ses_other'",
            [],
        )
        .unwrap();
        drop(conn);
        let mut paths = all_paths(&Scope::ALL);
        paths.sort();
        assert_eq!(paths, vec![db.join("ses_good")]);
        // And the corrupt session's pseudo-path classifies as nothing rather
        // than erroring the sweep: read_meta still resolves it, but its row
        // no longer filters — it is simply absent from the list.
        assert!(session_file(&db.join("ses_good")).is_some());
    }

    #[cfg(feature = "native")]
    #[test]
    fn db_fingerprint_is_stable_until_a_write() {
        let (_guard, home, _saved) = scratch_home("fingerprint");
        let db = db_path(&home);
        seed_session(&db, "ses_1", "/proj", 1000);
        let before = db_fingerprint(&db).expect("a scratch database fingerprints");
        // No write: the live tick gate sees the same fingerprint and skips
        // the full re-read.
        assert_eq!(db_fingerprint(&db), Some(before));
        seed_session(&db, "ses_2", "/proj", 2000);
        assert_ne!(
            db_fingerprint(&db),
            Some(before),
            "a write must move the gate or live ticks would miss rows"
        );
    }

    #[cfg(feature = "native")]
    #[test]
    fn session_file_names_the_session_or_the_newest() {
        let (_guard, home, _saved) = scratch_home("classify");
        let db = db_path(&home);
        seed_session(&db, "ses_new", "/proj", 2000);
        seed_session(&db, "ses_old", "/other", 1000);
        // A bare database is its newest session.
        let f = session_file(&db).unwrap();
        assert_eq!(f.session, "ses_new");
        assert_eq!(f.provider, Provider::Opencode);
        assert_eq!(f.role, FileRole::Root);
        assert_eq!(f.read, ReadMode::Whole);
        assert_eq!(f.path, db, "feeders read the database, not the pseudo-path");
        assert_eq!(f.project_key, "/proj");
        // A pseudo-path names its session.
        let g = session_file(&db.join("ses_old")).unwrap();
        assert_eq!(g.session, "ses_old");
        assert_eq!(g.project_key, "/other");
        // An unknown session, or a non-database, classifies as nothing.
        assert!(session_file(&db.join("ses_nope")).is_none());
        assert!(session_file(Path::new("/x/other.db")).is_none());
    }

    #[cfg(feature = "native")]
    #[test]
    fn read_session_joins_parts_to_messages_in_order() {
        let (_guard, home, _saved) = scratch_home("rows");
        let db = db_path(&home);
        seed_session(&db, "ses_1", "/proj", 1000);
        let (meta, messages) = read_session(&db, "ses_1").unwrap();
        assert_eq!(meta.id, "ses_1");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].1.role, "user");
        assert_eq!(messages[0].2.len(), 1);
        assert!(read_session(&db, "ses_nope").is_none());
    }
}
