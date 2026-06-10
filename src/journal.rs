//! Run journal for resume: each successful agent result is appended to
//! `runs/<run_id>.jsonl` keyed by a deterministic hash of its identity-bearing
//! inputs. On `--resume <run_id>` the file is loaded into a cache so re-running
//! the same workflow returns cached results instead of re-spawning codex.
//!
//! The key is FNV-1a (NOT std DefaultHasher, which is randomly seeded and would
//! never reproduce across processes) over a fixed-order canonical encoding of the
//! cache-relevant fields. Cosmetic/operational fields (id, label, step, timeout)
//! are excluded so they don't bust the cache.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The agent properties that determine cache identity. Excludes id/label/step
/// (cosmetic) and timeout_ms (operational), per SPEC M5.
#[derive(Debug, Clone, Copy)]
pub struct KeyInput<'a> {
    pub prompt: &'a str,
    pub model: Option<&'a str>,
    pub sandbox: Option<&'a str>,
    pub schema: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub isolate: bool,
}

/// 64-bit FNV-1a. Stable across processes (the whole point — a randomly-seeded
/// hasher would make resume never hit).
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Fnv1a(0xcbf2_9ce4_8422_2325)
    }
    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Deterministic cache key for one agent invocation. Field-name prefixes plus a
/// NUL separator make the encoding canonical AND collision-safe across field
/// boundaries (model="a",sandbox="b" must not equal model="ab",sandbox="");
/// the +/- presence tag keeps None distinct from Some("") (SPEC M5).
pub fn journal_key(k: &KeyInput) -> String {
    let mut h = Fnv1a::new();
    h.update(b"prompt=");
    h.update(k.prompt.as_bytes());
    update_opt(&mut h, b"\x00model", k.model);
    update_opt(&mut h, b"\x00sandbox", k.sandbox);
    update_opt(&mut h, b"\x00schema", k.schema);
    update_opt(&mut h, b"\x00cwd", k.cwd);
    h.update(b"\x00isolate=");
    h.update(if k.isolate { b"1" } else { b"0" });
    format!("{:016x}", h.finish())
}

fn update_opt(h: &mut Fnv1a, tag: &[u8], v: Option<&str>) {
    h.update(tag);
    match v {
        Some(s) => {
            h.update(b"+");
            h.update(s.as_bytes());
        }
        None => h.update(b"-"),
    }
}

/// One journal line: a cache key, its occurrence index, and the agent's final
/// text (string; JSON when a schema was set). Serialized as a single line so
/// concurrent appends can't interleave mid-record.
#[derive(Debug, Serialize, Deserialize)]
pub struct JournalEntry {
    pub key: String,
    /// The Nth call with this key, in call order. Keeps N identical (prompt,
    /// opts) calls — judge panels, adversarial votes — N independent samples
    /// instead of collapsing them into one cached result.
    #[serde(default)]
    pub occ: u32,
    pub result: String,
}

/// In-memory cache backed by a run's jsonl file.
pub struct Journal {
    cache: HashMap<(String, u32), String>,
    /// Occurrence counter per key for THIS process's call sequence.
    seen: HashMap<String, u32>,
}

impl Journal {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            seen: HashMap::new(),
        }
    }

    /// Load a run's journal into a key->result cache. A missing file is an empty
    /// journal (fresh run); any OTHER read error is surfaced so a --resume that
    /// would silently re-run everything can't happen. Malformed lines are
    /// skipped (a torn tail from a crashed run must not abort resume).
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let mut cache = HashMap::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(e) = serde_json::from_str::<JournalEntry>(line) {
                cache.insert((e.key, e.occ), e.result);
            }
        }
        Ok(Self {
            cache,
            seen: HashMap::new(),
        })
    }

    /// Claim the next occurrence index for `key` (0-based, call order). Call
    /// exactly once per agent() invocation, before `get`.
    pub fn occurrence(&mut self, key: &str) -> u32 {
        let n = self.seen.entry(key.to_string()).or_insert(0);
        let occ = *n;
        *n += 1;
        occ
    }

    /// Cached result for a (key, occurrence), if present (a resume hit).
    pub fn get(&self, key: &str, occ: u32) -> Option<&str> {
        self.cache.get(&(key.to_string(), occ)).map(|s| s.as_str())
    }

    /// Append one entry as a single whole line and update the cache. A key already
    /// present is a no-op (a resume hit must not re-append). The parent dir is
    /// created on demand; one `write_all` per line keeps concurrent appends from
    /// interleaving.
    pub fn append(
        &mut self,
        path: &Path,
        key: String,
        occ: u32,
        result: String,
    ) -> std::io::Result<()> {
        if self.cache.contains_key(&(key.clone(), occ)) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let entry = JournalEntry {
            key: key.clone(),
            occ,
            result: result.clone(),
        };
        let mut line = serde_json::to_string(&entry).unwrap_or_default();
        line.push('\n');
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)?;
        // Heal a torn tail from a crashed run (codex M5 review #2): without
        // this, the new record glues onto the partial line and BOTH are lost
        // to every future load. (Reads honor seek; appends still go to EOF.)
        if f.metadata()?.len() > 0 {
            let mut last = [0u8; 1];
            f.seek(SeekFrom::End(-1))?;
            f.read_exact(&mut last)?;
            if last[0] != b'\n' {
                f.write_all(b"\n")?;
            }
        }
        f.write_all(line.as_bytes())?;
        self.cache.insert((key, occ), result);
        Ok(())
    }
}

impl Default for Journal {
    fn default() -> Self {
        Self::new()
    }
}

/// Path to a run's journal file: `<CODEX_FLOW_RUNS_DIR or ./runs>/<run_id>.jsonl`.
pub fn journal_path(run_id: &str) -> PathBuf {
    let dir = std::env::var("CODEX_FLOW_RUNS_DIR").unwrap_or_else(|_| "runs".to_string());
    PathBuf::from(dir).join(format!("{run_id}.jsonl"))
}

/// Locally-unique run id: start-time seconds + pid, both hex. No uuid/RNG dep;
/// a collision needs two runs in the same second with a recycled pid. (The
/// determinism ban applies to workflow SCRIPTS, not the host.)
pub fn new_run_id() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs:x}-{:x}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> KeyInput<'static> {
        KeyInput {
            prompt: "do x",
            model: Some("gpt"),
            sandbox: Some("read-only"),
            schema: None,
            cwd: None,
            isolate: false,
        }
    }

    #[test]
    fn journal_key_deterministic_and_field_sensitive() {
        let b = base();
        assert_eq!(journal_key(&b), journal_key(&b), "must be deterministic");
        // Every cache-relevant field flips the key.
        let mut p = b;
        p.prompt = "do y";
        assert_ne!(journal_key(&p), journal_key(&b));
        let mut m = b;
        m.model = Some("o3");
        assert_ne!(journal_key(&m), journal_key(&b));
        let mut s = b;
        s.schema = Some("{}");
        assert_ne!(journal_key(&s), journal_key(&b));
        let mut c = b;
        c.cwd = Some("/tmp");
        assert_ne!(journal_key(&c), journal_key(&b));
        let mut i = b;
        i.isolate = true;
        assert_ne!(journal_key(&i), journal_key(&b));
    }

    #[test]
    fn none_and_empty_string_differ() {
        let none = KeyInput { schema: None, ..base() };
        let empty = KeyInput {
            schema: Some(""),
            ..base()
        };
        assert_ne!(
            journal_key(&none),
            journal_key(&empty),
            "None must not collide with Some(\"\") (SPEC M5)"
        );
    }

    #[test]
    fn journal_key_no_field_boundary_collision() {
        // The classic concat bug: "a"+"b" must not collide with "ab"+"".
        let a = KeyInput {
            model: Some("a"),
            sandbox: Some("b"),
            ..base()
        };
        let c = KeyInput {
            model: Some("ab"),
            sandbox: Some(""),
            ..base()
        };
        assert_ne!(
            journal_key(&a),
            journal_key(&c),
            "NUL+name separators must prevent boundary collisions"
        );
    }

    #[test]
    fn journal_append_then_load_roundtrips_and_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let mut j = Journal::new();
        j.append(&path, "k1".into(), 0, "r1".into()).unwrap();
        j.append(&path, "k2".into(), 0, "{\"a\":1}".into()).unwrap();
        // Re-appending an existing (key, occ) is a no-op (resume hit must not duplicate).
        j.append(&path, "k1".into(), 0, "DIFFERENT".into()).unwrap();
        // A later occurrence of the same key IS a distinct entry.
        j.append(&path, "k1".into(), 1, "second sample".into()).unwrap();

        let loaded = Journal::load(&path).unwrap();
        assert_eq!(loaded.get("k1", 0), Some("r1"));
        assert_eq!(loaded.get("k1", 1), Some("second sample"));
        assert_eq!(loaded.get("k2", 0), Some("{\"a\":1}"));
        assert_eq!(loaded.get("missing", 0), None);
        // Exactly three physical lines: the duplicate (k1, 0) was not written.
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 3);
    }

    #[test]
    fn torn_tail_is_healed_on_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        // A crashed run left a partial record with no trailing newline.
        std::fs::write(&path, "{\"key\":\"k0\",\"occ\":0,\"result\":\"trunc").unwrap();
        let mut j = Journal::new();
        j.append(&path, "k1".into(), 0, "good".into()).unwrap();
        let loaded = Journal::load(&path).unwrap();
        assert_eq!(loaded.get("k1", 0), Some("good"), "new record survives the torn tail");
        assert_eq!(loaded.get("k0", 0), None, "torn record skipped, not merged");
    }

    #[test]
    fn occurrence_distinguishes_identical_calls() {
        let mut j = Journal::new();
        assert_eq!(j.occurrence("k"), 0);
        assert_eq!(j.occurrence("k"), 1, "second identical call gets occ 1");
        assert_eq!(j.occurrence("other"), 0);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let j = Journal::load(Path::new("/nonexistent/path/run.jsonl")).unwrap();
        assert_eq!(j.get("anything", 0), None);
    }

    #[test]
    fn run_ids_are_wellformed() {
        let id = new_run_id();
        let (secs, pid) = id.split_once('-').expect("secs-pid form");
        assert!(u64::from_str_radix(secs, 16).is_ok());
        assert!(u32::from_str_radix(pid, 16).is_ok());
    }
}
