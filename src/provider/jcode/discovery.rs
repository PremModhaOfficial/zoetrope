//! Where jcode keeps a session on disk, and how to tell what a file is:
//! `~/.jcode/sessions/session_<name>_<ts>_<hex>.json`, one JSON document per
//! session, rewritten on every turn. Pure path logic plus directory scans,
//! except [`session_file`], which reads the document's top-level fields (its
//! id and working directory live inside it, not in the path).
//!
//! A session is exactly one file: the `session_*.journal.jsonl` append logs
//! beside it replay messages that are already in the document, so they are
//! not part of any session and are never tailed. `related_paths` is empty.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::wire::{Document, parse_doc};
use crate::provider::{FileRole, Provider, ReadMode, Scope, SessionFile};

/// The jcode home: `$JCODE_HOME`, else `~/.jcode`.
pub fn jcode_home() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("JCODE_HOME") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".jcode"))
}

/// The sessions directory under a jcode home.
pub fn sessions_dir(home: &Path) -> PathBuf {
    home.join("sessions")
}

/// Whether a path is shaped like a session document: `session_*.json`. Shape
/// only; [`session_file`] decides what it is. This rejects the `.bak`
/// snapshots (a different extension) and the `.journal.jsonl` append logs.
pub fn is_session_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("json")
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("session_"))
}

/// Every session document under the sessions directory, in path order. The
/// project cannot prune by path — the working directory is inside the
/// document — so a project scope returns everything recent and the core
/// filters by [`project_key`](Provider::project_key); the id scope prunes by
/// file name instead, since the file stem is the session id.
pub fn all_paths(scope: &Scope) -> Vec<PathBuf> {
    let Some(home) = jcode_home() else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(sessions_dir(&home)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() || !is_session_file(&path) {
            continue;
        }
        if let Some(since) = scope.since
            && crate::provider::modified(&path) < since
        {
            continue;
        }
        // The file stem is the session id: prune without reading.
        if let Some(prefix) = &scope.id_prefix
            && !path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with(prefix.as_str()))
        {
            continue;
        }
        out.push(path);
    }
    out.sort();
    out
}

/// What a session document is, by its own top-level fields. The document id
/// is the session; the working directory is the project key. A torn mid-write
/// read parses to nothing and is retried on the next tick.
pub fn session_file(path: &Path) -> Option<SessionFile> {
    if !is_session_file(path) {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    from_doc(path, &parse_doc(&text)?, crate::provider::modified(path))
}

fn from_doc(path: &Path, doc: &Document, modified: SystemTime) -> Option<SessionFile> {
    // The document id is the session. A file shaped like a session document
    // whose JSON carries no id is skipped, not joined by its stem as a
    // phantom empty session: any `session_*.json` with parseable JSON but no
    // messages or id (a torn shape, a foreign file) would otherwise become a
    // session with nothing in it. Like Claude — whose session id comes from
    // the path, never the content — a missing identity means no session.
    // Census 2026-10-05: all 391 live session documents carry an id.
    let session = doc.id.clone()?;
    Some(SessionFile {
        provider: Provider::Jcode,
        path: path.to_path_buf(),
        session,
        role: FileRole::Root,
        read: ReadMode::Doc,
        project_key: doc.working_dir.clone().unwrap_or_default(),
        modified,
    })
}

/// [`session_file`] without a filesystem: pure path shape, so the browser can
/// classify a dropped file by the path it came with. The content already said
/// this is jcode's (see [`Provider::session_file_from`](crate::provider::Provider::session_file_from)),
/// so the session is the file stem — which is the document id — and the
/// project key is unknown until the document itself is read.
pub fn classify_path(path: &Path, modified: SystemTime) -> Option<SessionFile> {
    if !is_session_file(path) {
        return None;
    }
    Some(SessionFile {
        provider: Provider::Jcode,
        path: path.to_path_buf(),
        session: path.file_stem()?.to_str()?.to_string(),
        role: FileRole::Root,
        read: ReadMode::Doc,
        project_key: String::new(),
        modified,
    })
}

/// Where the rest of a file's session is: nowhere. A jcode session is its one
/// document — no subagent transcripts, no sidecars worth tailing.
pub fn related_paths(_file: &SessionFile) -> Vec<PathBuf> {
    Vec::new()
}

/// jcode records the working directory as it is.
pub fn project_key(cwd: &Path) -> String {
    cwd.to_string_lossy().into_owned()
}

/// The document stream for a session file: it diffs each rewrite against the
/// message ids it has already stated.
pub fn stream_for(_file: &SessionFile) -> super::Stream {
    super::Stream::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_shape_filter() {
        assert!(is_session_file(Path::new(
            "/h/sessions/session_ant_1785579094110_8dc71dda663c4c58.json"
        )));
        // Rejected: backups, append journals, wrong prefixes and extensions.
        assert!(!is_session_file(Path::new(
            "/h/sessions/session_ant_1785579094110_8dc71dda663c4c58.bak"
        )));
        assert!(!is_session_file(Path::new(
            "/h/sessions/session_bear_1787388688883_25918d9b411e6b28.journal.jsonl"
        )));
        assert!(!is_session_file(Path::new("/h/sessions/notes.json")));
        assert!(!is_session_file(Path::new("/h/sessions/session_x.jsonl")));
    }

    #[test]
    fn classify_path_reads_the_stem() {
        let f = classify_path(
            Path::new("/h/sessions/session_demo_1_a.json"),
            SystemTime::UNIX_EPOCH,
        )
        .expect("classifies");
        assert_eq!(f.provider, Provider::Jcode);
        assert_eq!(f.session, "session_demo_1_a");
        assert_eq!(f.role, FileRole::Root);
        assert_eq!(f.read, ReadMode::Doc);
        assert!(classify_path(Path::new("/h/notes.json"), SystemTime::UNIX_EPOCH).is_none());
    }

    #[test]
    fn project_key_is_the_cwd_verbatim() {
        assert_eq!(project_key(Path::new("/project")), "/project");
    }

    #[test]
    fn document_without_an_id_is_no_session() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("zoetrope_jcode_noid_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Shaped like a session document, parses, but carries no id: a
        // phantom if joined by stem, so it must classify as nothing.
        let path = dir.join("session_phantom_1_a.json");
        std::fs::write(&path, r#"{"working_dir":"/p","messages":[]}"#).unwrap();
        assert!(session_file(&path).is_none());
        // The same file with an id is its session.
        std::fs::write(
            &path,
            r#"{"id":"session_phantom_1_a","working_dir":"/p","messages":[]}"#,
        )
        .unwrap();
        let f = session_file(&path).expect("classifies with an id");
        assert_eq!(f.session, "session_phantom_1_a");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
