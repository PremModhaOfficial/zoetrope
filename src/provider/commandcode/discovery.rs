//! Where Command Code keeps its sessions on disk, and how to tell what a
//! file is: `~/.commandcode/projects/<slug(cwd)>/<uuid>.jsonl`. One file per
//! session — children are never persisted (their turns are embedded in the
//! parent's own transcript), so nothing here ever returns more than the one
//! path. Pure path logic, directory scans and at most a head read.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::wire;
use crate::provider::{FileRole, Provider, ReadMode, Scope, SessionFile};

// ---------------------------------------------------------------------------
// Roots
// ---------------------------------------------------------------------------

/// The Command Code home: `$COMMANDCODE_HOME`, else `~/.commandcode`.
pub fn commandcode_home() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("COMMANDCODE_HOME") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".commandcode"))
}

/// The directory every session lives under, one project directory deep.
pub fn projects_root() -> Option<PathBuf> {
    Some(commandcode_home()?.join("projects"))
}

// ---------------------------------------------------------------------------
// The slug rule
// ---------------------------------------------------------------------------

/// Sanitize an absolute cwd into the project-directory name Command Code
/// uses. The verified rule (33/33 real dir↔cwd pairs):
///
/// 1. split camel/acronym boundaries: `([A-Z]+)([A-Z][a-z])` and
///    `([a-z0-9])([A-Z])` each become `$1-$2`;
/// 2. lowercase;
/// 3. every run of `[^a-z0-9]+` becomes a single `-`;
/// 4. trim leading and trailing `-`.
///
/// Runs are *collapsed* and the result trimmed — a component literally named
/// `-home-prm-leya` must not produce a double dash — and the leading `/` is
/// stripped by step 3 rather than surviving as a dash.
pub fn slug(cwd: &Path) -> String {
    let original: Vec<char> = cwd.to_string_lossy().chars().collect();
    // Step 1a: split an acronym run from the word that follows it
    // (`HCLtech` → `HC-Ltech`).
    let mut split = Vec::with_capacity(original.len() + 8);
    for (i, &c) in original.iter().enumerate() {
        if i >= 1
            && i + 1 < original.len()
            && original[i - 1].is_ascii_uppercase()
            && c.is_ascii_uppercase()
            && original[i + 1].is_ascii_lowercase()
        {
            split.push('-');
        }
        split.push(c);
    }
    // Step 1b: split a lower/digit from an upper (`dynamicArr` →
    // `dynamic-Arr`). Applied after 1a, like the two regex passes.
    let mut split2 = Vec::with_capacity(split.len() + 8);
    for (i, &c) in split.iter().enumerate() {
        if i >= 1
            && (split[i - 1].is_ascii_lowercase() || split[i - 1].is_ascii_digit())
            && c.is_ascii_uppercase()
        {
            split2.push('-');
        }
        split2.push(c);
    }
    // Steps 2–4 in one pass: lowercase, collapse every non-alphanumeric run
    // to one dash, drop leading/trailing dashes.
    let mut out = String::with_capacity(split2.len());
    let mut pending_dash = false;
    for c in split2 {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Whether a filename stem is a canonical UUID (8-4-4-4-12 hex). The sweep
/// shape filter: session files are exactly `<uuid>.jsonl`.
fn is_uuid(stem: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let mut parts = stem.split('-');
    for &len in &GROUPS {
        match parts.next() {
            Some(p) if p.len() == len && p.bytes().all(|b| b.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

/// Sweep shape filter: a bare `<uuid>.jsonl` directly inside a project dir.
/// This rejects, by construction, every sidecar suffix
/// (`*.checkpoints.jsonl`, `*.prompts.jsonl`, `*.meta.json`,
/// `*.scheduled-tasks.json`, …), `hooks-audit-<uuid>.jsonl` (its stem is not
/// a bare UUID) and `config.json`/`mcp.json`. It is only the sweep's cheap
/// filter; the header is the authority.
pub fn is_session_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("jsonl")
        && path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(is_uuid)
}

/// The first non-blank line of a file, if it can be read at all.
fn head_line(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    while reader.read_line(&mut line).ok()? > 0 {
        if !line.trim().is_empty() {
            return Some(line);
        }
        line.clear();
    }
    None
}

/// What a path is, reading its head. The session id is the header's; the
/// stem is only a fallback, because 1/165 real files disagree with their
/// stem (`SESSION-FILE-RULE`).
pub fn session_file(path: &Path) -> Option<SessionFile> {
    let head = head_line(path).unwrap_or_default();
    classify(path, &head, crate::provider::modified(path))
}

/// [`session_file`] without reading the file: the path plus its first bytes,
/// so the browser can classify a dropped file. Still uses the environment —
/// the provisional case needs to know where `projects/` is.
pub fn classify_head(path: &Path, head: &str, modified: SystemTime) -> Option<SessionFile> {
    classify(path, head, modified)
}

/// The one classification rule, total: every branch returns `Option`.
fn classify(path: &Path, head: &str, modified: SystemTime) -> Option<SessionFile> {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return None;
    }
    let name = path.file_name().and_then(|n| n.to_str())?;
    if name.ends_with(".checkpoints.jsonl") || name.ends_with(".prompts.jsonl") {
        return None;
    }
    let stem = path.file_stem().and_then(|s| s.to_str());
    let project_key = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let file = |session: String| SessionFile {
        provider: Provider::CommandCode,
        path: path.to_path_buf(),
        session,
        role: FileRole::Root,
        read: ReadMode::Tail,
        project_key: project_key.clone(),
        modified,
    };
    // The header is the authority: a first non-blank line with `type ==
    // "session"` and a numeric `version` names the session (its id, not the
    // stem), whatever the file is called.
    let first = head.lines().find(|l| !l.trim().is_empty());
    if let Some(header) = first.and_then(wire::parse_header) {
        let session = header
            .id
            .filter(|id| !id.is_empty())
            .or_else(|| stem.map(str::to_owned))?;
        return Some(file(session));
    }
    // No header yet: a `.jsonl` directly inside a project directory claims
    // provisionally with its stem, so a just-created session still opens as
    // Command Code until its header lands.
    let root = projects_root()?;
    if path.parent().and_then(Path::parent) == Some(root.as_path()) {
        return stem.map(str::to_owned).map(file);
    }
    None
}

// ---------------------------------------------------------------------------
// The provider primitives (see `provider/mod.rs` and docs/DISCOVERY.md)
// ---------------------------------------------------------------------------

/// Every session-shaped file this provider has, or the one project
/// directory a scope names. Filename pruning (`since`, `id_prefix`) is
/// best-effort: the id is the header's, and a stem can disagree.
pub fn all_paths(scope: &Scope) -> Vec<PathBuf> {
    let Some(root) = projects_root() else {
        return Vec::new();
    };
    let dirs: Vec<PathBuf> = match &scope.project {
        Some(cwd) => vec![root.join(slug(cwd))],
        None => std::fs::read_dir(&root)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default(),
    };
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !is_session_file(&path) {
                continue;
            }
            if let Some(since) = scope.since
                && crate::provider::modified(&path) < since
            {
                continue;
            }
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
    }
    out.sort();
    out
}

/// A session is one file: nothing else belongs to it. Children are embedded
/// in the parent's transcript, and the sidecars are not transcripts.
pub fn related_paths(_file: &SessionFile) -> Vec<PathBuf> {
    Vec::new()
}

/// Command Code names a project by the slug of its working directory, on
/// both sides of the `Scope::project` comparison.
pub fn project_key(cwd: &Path) -> String {
    slug(cwd)
}

/// A parser for one of this provider's files. Single-file sessions: no
/// per-role variants, so a fresh stream always.
pub fn stream_for(_file: &SessionFile) -> super::Stream {
    super::Stream::new()
}

/// Command Code has no whole-read files.
pub fn sidecar(_file: &SessionFile, _text: &str) -> Option<crate::fact::Statement> {
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::commandcode::test_env::EnvGuard;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zoetrope-commandcode-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const HEADER: &str = r#"{"type":"session","version":3,"id":"0e599cbe-23c4-460b-b097-cbd1d6bc0e3d","timestamp":"2026-09-30T10:00:00.000Z","cwd":"/home/prm/zoet/zoetrope"}"#;

    #[test]
    fn slug_matches_frozen_vectors() {
        for (input, expected) in [
            ("/home/prm/zoet", "home-prm-zoet"),
            ("/home/prm/zoet/zoetrope", "home-prm-zoet-zoetrope"),
            ("/home/prm/project/HCLtech", "home-prm-project-hc-ltech"),
            (
                "/home/prm/projects/unsortedProjects",
                "home-prm-projects-unsorted-projects",
            ),
            (
                "/home/prm/projects/neet/dynamicArr",
                "home-prm-projects-neet-dynamic-arr",
            ),
            ("/home/prm/.config/nvim", "home-prm-config-nvim"),
            (
                "/home/prm/dotfiles-agents/agents_skills",
                "home-prm-dotfiles-agents-agents-skills",
            ),
            ("/tmp", "tmp"),
            ("/tmp/-home-prm-leya/x", "tmp-home-prm-leya-x"),
        ] {
            assert_eq!(slug(Path::new(input)), expected, "slug({input})");
        }
        // #10: a trailing slash is the same project.
        assert_eq!(
            slug(Path::new("/home/prm/zoet/")),
            slug(Path::new("/home/prm/zoet"))
        );
        // #11 and #12 are UNVERIFIED (rule-derived): stated values, no panic.
        assert_eq!(slug(Path::new("v2.1")), "v2-1");
        assert_eq!(slug(Path::new("/")), "");
    }

    #[test]
    fn is_session_file_accepts_only_bare_uuid_jsonl() {
        const UUID: &str = "0e599cbe-23c4-460b-b097-cbd1d6bc0e3d";
        let accepts = format!("/p/{UUID}.jsonl");
        assert!(is_session_file(Path::new(&accepts)), "the one shape");
        for rejected in [
            "/p/hooks-audit-39da6371-7985-4782-9f23-63c79034f470.jsonl".to_string(),
            format!("/p/{UUID}.checkpoints.jsonl"),
            format!("/p/{UUID}.prompts.jsonl"),
            format!("/p/{UUID}.meta.json"),
            format!("/p/{UUID}.share.json"),
            format!("/p/{UUID}.scheduled-tasks.json"),
            "/p/mcp.json".to_string(),
            "/p/config.json".to_string(),
            "/p/journal.jsonl".to_string(),
            format!("/p/{UUID}.json"),
            format!("/p/{UUID}-extra.jsonl"),
        ] {
            assert!(
                !is_session_file(Path::new(&rejected)),
                "rejected: {rejected}"
            );
        }
    }

    #[test]
    fn session_file_reads_the_header_id_not_the_stem() {
        let _lock = EnvGuard::locked();
        let dir = temp_dir("header-id");

        // Stem and header disagree (1/165 in real data): the header wins.
        let stem_mismatch = dir.join("cc410ee0-1111-4111-8111-111111111111.jsonl");
        std::fs::write(
            &stem_mismatch,
            b"{\"type\":\"session\",\"version\":3,\"id\":\"9f5a40a2-2222-4222-8222-222222222222\",\"cwd\":\"/project\"}\n",
        )
        .unwrap();
        let f = session_file(&stem_mismatch).expect("a header classifies");
        assert_eq!(f.session, "9f5a40a2-2222-4222-8222-222222222222");
        assert_eq!(f.role, FileRole::Root);
        assert_eq!(f.read, ReadMode::Tail);
        assert_eq!(f.provider, Provider::CommandCode);
        assert_eq!(
            f.project_key,
            dir.file_name().unwrap().to_str().unwrap(),
            "the containing project directory's name"
        );

        // `hooks-audit-*` is a valid session whose stem is not a UUID; it
        // classifies by its header and stays openable by explicit path.
        let hooks = dir.join("hooks-audit-39da6371-7985-4782-9f23-63c79034f470.jsonl");
        std::fs::write(
            &hooks,
            b"{\"type\":\"session\",\"version\":3,\"id\":\"hooks-audit-39da6371-7985-4782-9f23-63c79034f470\",\"cwd\":\"/project\"}\n",
        )
        .unwrap();
        let f = session_file(&hooks).expect("a valid header claims any name");
        assert_eq!(
            f.session,
            "hooks-audit-39da6371-7985-4782-9f23-63c79034f470"
        );

        // No header, not under a projects tree: no claim.
        let notes = dir.join("notes.jsonl");
        std::fs::write(&notes, b"{\"hello\":\"world\"}\n").unwrap();
        assert!(session_file(&notes).is_none());
        // Sidecars never claim, header or not.
        let checkpoints = dir.join("0e599cbe-23c4-460b-b097-cbd1d6bc0e3d.checkpoints.jsonl");
        std::fs::write(&checkpoints, b"{\"id\":\"c1\",\"messageId\":\"m1\"}\n").unwrap();
        assert!(session_file(&checkpoints).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn related_paths_is_empty_for_every_role() {
        let file = |role| SessionFile {
            provider: Provider::CommandCode,
            path: PathBuf::from("/p/x/y.jsonl"),
            session: "y".into(),
            role,
            read: ReadMode::Tail,
            project_key: "p".into(),
            modified: SystemTime::UNIX_EPOCH,
        };
        assert!(related_paths(&file(FileRole::Root)).is_empty());
        assert!(related_paths(&file(FileRole::Agent { parent: "y".into() })).is_empty());
        assert!(related_paths(&file(FileRole::Sidecar)).is_empty());
    }

    #[test]
    fn project_key_is_the_slug_on_both_sides() {
        let path = Path::new(
            "/tmp/projects/home-prm-zoet-zoetrope/0e599cbe-23c4-460b-b097-cbd1d6bc0e3d.jsonl",
        );
        let f = classify_head(path, HEADER, SystemTime::UNIX_EPOCH).expect("header classifies");
        assert_eq!(f.project_key, "home-prm-zoet-zoetrope");
        assert_eq!(
            project_key(Path::new("/home/prm/zoet/zoetrope")),
            "home-prm-zoet-zoetrope"
        );
        assert_eq!(
            project_key(Path::new("/home/prm/zoet/zoetrope/")),
            f.project_key,
            "trailing slash admits the same project"
        );
    }

    #[test]
    fn classify_head_is_pure_and_reads_the_head() {
        // The path need not exist: only the head string classifies.
        let path = Path::new("/fixtures/demo.jsonl");
        let f = classify_head(path, HEADER, SystemTime::UNIX_EPOCH).expect("header classifies");
        assert_eq!(f.provider, Provider::CommandCode);
        assert_eq!(f.session, "0e599cbe-23c4-460b-b097-cbd1d6bc0e3d");
        assert_eq!(f.project_key, "fixtures");
        assert_eq!(f.modified, SystemTime::UNIX_EPOCH);

        // A non-header head outside any projects tree claims nothing.
        let _lock = EnvGuard::locked();
        assert!(classify_head(path, "{\"hello\":\"world\"}", SystemTime::UNIX_EPOCH).is_none());
        assert!(classify_head(path, "", SystemTime::UNIX_EPOCH).is_none());
    }

    #[test]
    fn all_paths_prunes_by_scope_and_claims_provisionally() {
        let tmp = temp_dir("all-paths");
        let home = tmp.join("home");
        let project = home.join("projects").join("home-prm-zoet-zoetrope");
        std::fs::create_dir_all(&project).unwrap();
        let session = project.join("0e599cbe-23c4-460b-b097-cbd1d6bc0e3d.jsonl");
        std::fs::write(&session, format!("{HEADER}\n")).unwrap();
        let empty = project.join("77777777-7777-4777-8777-777777777777.jsonl");
        std::fs::write(&empty, b"").unwrap();
        // Every non-session shape a real project dir contains.
        for noise in [
            "config.json",
            "mcp.json",
            "0e599cbe-23c4-460b-b097-cbd1d6bc0e3d.checkpoints.jsonl",
            "0e599cbe-23c4-460b-b097-cbd1d6bc0e3d.meta.json",
            "hooks-audit-39da6371-7985-4782-9f23-63c79034f470.jsonl",
        ] {
            std::fs::write(project.join(noise), b"{}\n").unwrap();
        }

        let _guard = EnvGuard::with_home(&home);

        // Only the bare-uuid files, sorted.
        assert_eq!(all_paths(&Scope::ALL), vec![session.clone(), empty.clone()]);
        // Project scope narrows to `projects/<slug(cwd)>`.
        assert_eq!(
            all_paths(&Scope::project(Path::new("/home/prm/zoet/zoetrope"))),
            vec![session.clone(), empty.clone()]
        );
        assert!(all_paths(&Scope::project(Path::new("/elsewhere"))).is_empty());
        // Id-prefix pruning by stem (best effort; nothing is read).
        assert_eq!(all_paths(&Scope::id("0e59")), vec![session.clone()]);
        assert!(all_paths(&Scope::id("aaaa")).is_empty());
        // `since` pruning by modification time.
        assert_eq!(
            all_paths(&Scope::ALL.since(SystemTime::UNIX_EPOCH)).len(),
            2
        );
        let future = SystemTime::now() + std::time::Duration::from_secs(3_600);
        assert!(all_paths(&Scope::ALL.since(future)).is_empty());

        // A headerless file directly in a project dir claims provisionally
        // with its stem; the header file's own id is the authority.
        let provisional = session_file(&empty).expect("claims provisionally");
        assert_eq!(provisional.session, "77777777-7777-4777-8777-777777777777");
        assert_eq!(provisional.role, FileRole::Root);
        assert_eq!(provisional.project_key, "home-prm-zoet-zoetrope");
        assert_eq!(
            session_file(&session).unwrap().session,
            "0e599cbe-23c4-460b-b097-cbd1d6bc0e3d"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sidecar_states_nothing() {
        let f = SessionFile {
            provider: Provider::CommandCode,
            path: PathBuf::from("/p/x.jsonl"),
            session: "x".into(),
            role: FileRole::Root,
            read: ReadMode::Tail,
            project_key: "p".into(),
            modified: SystemTime::UNIX_EPOCH,
        };
        assert!(sidecar(&f, "{}").is_none());
        assert!(sidecar(&f, "").is_none());
    }

    #[test]
    fn demo_fixture_classifies_as_a_root() {
        let Some(dir) = crate::provider::harness::fixture_dir("commandcode") else {
            return;
        };
        let demo = dir.join("demo.jsonl");
        let f = session_file(&demo).expect("the fixture's header classifies");
        assert_eq!(f.provider, Provider::CommandCode);
        assert_eq!(f.session, "demo");
        assert_eq!(f.role, FileRole::Root);
        assert_eq!(f.read, ReadMode::Tail);
        assert_eq!(f.project_key, "commandcode");
    }

    // S-004
    #[test]
    fn s_004_slug_matches_frozen_vectors() {
        let vectors = [
            ("/home/prm/zoet", "home-prm-zoet"),
            ("/home/prm/zoet/zoetrope", "home-prm-zoet-zoetrope"),
            ("/home/prm/project/HCLtech", "home-prm-project-hc-ltech"),
            (
                "/home/prm/projects/unsortedProjects",
                "home-prm-projects-unsorted-projects",
            ),
            (
                "/home/prm/projects/neet/dynamicArr",
                "home-prm-projects-neet-dynamic-arr",
            ),
            ("/home/prm/.config/nvim", "home-prm-config-nvim"),
            (
                "/home/prm/dotfiles-agents/agents_skills",
                "home-prm-dotfiles-agents-agents-skills",
            ),
            ("/tmp", "tmp"),
            ("/tmp/-home-prm-leya/x", "tmp-home-prm-leya-x"),
            // #10: a trailing slash is the same project.
            ("/home/prm/zoet/", "home-prm-zoet"),
            // #11 and #12 are the design's UNVERIFIED rule-derived cases.
            ("v2.1", "v2-1"),
            ("/", ""),
        ];
        for (input, expected) in vectors {
            assert_eq!(slug(Path::new(input)), expected, "slug({input})");
        }
    }

    // S-005
    #[test]
    fn s_005_is_session_file_rejects_sidecars_and_hooks_audit() {
        const UUID: &str = "0e599cbe-23c4-460b-b097-cbd1d6bc0e3d";
        let accepted = format!("/p/{UUID}.jsonl");
        let rejected = [
            "/p/hooks-audit-39da6371-7985-4782-9f23-63c79034f470.jsonl".to_string(),
            format!("/p/{UUID}.checkpoints.jsonl"),
            format!("/p/{UUID}.prompts.jsonl"),
            format!("/p/{UUID}.meta.json"),
            format!("/p/{UUID}.share.json"),
            format!("/p/{UUID}.scheduled-tasks.json"),
            "/p/mcp.json".to_string(),
            "/p/config.json".to_string(),
            "/p/journal.jsonl".to_string(),
            format!("/p/{UUID}.json"),
            format!("/p/{UUID}-extra.jsonl"),
        ];
        assert!(is_session_file(Path::new(&accepted)), "the one shape");
        for path in rejected {
            assert!(!is_session_file(Path::new(&path)), "rejected: {path}");
        }
    }

    // S-006
    #[test]
    fn s_006_session_file_reads_header_id_not_stem() {
        let _lock = EnvGuard::locked();
        let dir = temp_dir("s006-header-id");

        let mismatch = dir.join("cc410ee0-1111-4111-8111-111111111111.jsonl");
        std::fs::write(
            &mismatch,
            b"{\"type\":\"session\",\"version\":3,\"id\":\"9f5a40a2-2222-4222-8222-222222222222\",\"cwd\":\"/project\"}\n",
        )
        .unwrap();
        let f = session_file(&mismatch).expect("a valid header claims the file");
        assert_eq!(f.session, "9f5a40a2-2222-4222-8222-222222222222");
        assert_eq!(f.role, FileRole::Root);
        assert_eq!(f.read, ReadMode::Tail);
        assert_eq!(f.provider, Provider::CommandCode);
        assert_eq!(f.project_key, dir.file_name().unwrap().to_str().unwrap());

        let hooks = dir.join("hooks-audit-39da6371-7985-4782-9f23-63c79034f470.jsonl");
        std::fs::write(
            &hooks,
            b"{\"type\":\"session\",\"version\":3,\"id\":\"hooks-audit-39da6371-7985-4782-9f23-63c79034f470\",\"cwd\":\"/project\"}\n",
        )
        .unwrap();
        let f = session_file(&hooks).expect("a valid header claims any name");
        assert_eq!(
            f.session,
            "hooks-audit-39da6371-7985-4782-9f23-63c79034f470"
        );

        // No header, not directly inside a projects tree: no claim.
        let notes = dir.join("notes.jsonl");
        std::fs::write(&notes, b"{\"hello\":\"world\"}\n").unwrap();
        assert!(session_file(&notes).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // S-007
    #[test]
    fn s_007_related_paths_empty_and_project_key_is_slug() {
        let tmp = temp_dir("s007-project-key");
        let project = tmp.join("projects").join("home-prm-zoet-zoetrope");
        std::fs::create_dir_all(&project).unwrap();
        let path = project.join("0e599cbe-23c4-460b-b097-cbd1d6bc0e3d.jsonl");
        let head = r#"{"type":"session","version":3,"id":"0e599cbe-23c4-460b-b097-cbd1d6bc0e3d","timestamp":"2026-09-30T10:00:00.000Z","cwd":"/home/prm/zoet/zoetrope"}"#;
        std::fs::write(&path, format!("{head}\n")).unwrap();

        let f = session_file(&path).expect("the header classifies");
        assert_eq!(related_paths(&f), Vec::<PathBuf>::new(), "single-file");
        let typed = Provider::CommandCode.project_key(Path::new("/home/prm/zoet/zoetrope"));
        assert_eq!(typed, "home-prm-zoet-zoetrope");
        assert_eq!(typed, f.project_key);
        assert_eq!(
            project_key(Path::new("/home/prm/zoet/zoetrope/")),
            f.project_key,
            "a trailing slash is the same project"
        );

        let classified = classify_head(&path, head, SystemTime::UNIX_EPOCH)
            .expect("the browser path classifies the head");
        assert_eq!(classified.session, f.session);
        assert_eq!(classified.role, FileRole::Root);
        assert_eq!(classified.read, ReadMode::Tail);
        assert_eq!(classified.project_key, f.project_key);
        assert_eq!(classified.modified, SystemTime::UNIX_EPOCH);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
