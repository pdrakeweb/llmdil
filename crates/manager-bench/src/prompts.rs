//! The prompt library (design §4.1–§4.2): purpose-keyed entries,
//! shipped versioned defaults compiled into the crate, and a
//! serde store at `<data_dir>/bench-prompts.json`. Shipped entries
//! materialize into the store on first read and refresh on app
//! update when untouched; an entry the operator edited (its origin
//! flipped to `user` by the editor) is never overwritten.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::comparability::prompt_sha256;

/// Where a prompt entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptOrigin {
    #[serde(rename = "shipped")]
    Shipped,
    #[serde(rename = "user")]
    User,
    #[serde(rename = "generated")]
    Generated,
}

/// One library entry. `sha256` is always the hex sha256 of `body`
/// for entries the library itself produces; a sweep pins the body
/// by this hash (§3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PromptEntry {
    pub id: String,
    pub name: String,
    /// Purpose tag the library is keyed by: `coder`, `planner`,
    /// `tool-runner`, `general`, or an operator-created tag.
    pub purpose: String,
    pub body: String,
    pub sha256: String,
    pub origin: PromptOrigin,
    /// Entry version; shipped entries bump it when their text
    /// changes so [`PromptLibrary::materialize_shipped`] can refresh
    /// untouched copies.
    pub version: u32,
    /// Last edit, wall-clock milliseconds since the Unix epoch.
    /// 0 for shipped entries as compiled in.
    pub updated_ms: i64,
}

impl PromptEntry {
    /// Build an entry, computing its hash from its body.
    fn new(
        id: &str,
        name: &str,
        purpose: &str,
        body: String,
        origin: PromptOrigin,
        version: u32,
        updated_ms: i64,
    ) -> Self {
        let sha256 = prompt_sha256(&body);
        PromptEntry {
            id: id.to_string(),
            name: name.to_string(),
            purpose: purpose.to_string(),
            body,
            sha256,
            origin,
            version,
            updated_ms,
        }
    }
}

/// Current version of every shipped prompt entry.
const SHIPPED_PROMPT_VERSION: u32 = 1;

/// Current on-disk format version of the prompt library file.
const PROMPT_LIBRARY_FORMAT_VERSION: u32 = 1;

const CODER_STANDARD_BODY: &str = include_str!("../prompts/coder_standard.txt");
const CODER_LONG_BODY: &str = include_str!("../prompts/coder_long.txt");
const PLANNER_STANDARD_BODY: &str = include_str!("../prompts/planner_standard.txt");
const TOOL_STANDARD_BODY: &str = include_str!("../prompts/tool_standard.txt");
const GENERAL_STANDARD_BODY: &str = include_str!("../prompts/general_standard.txt");

/// The five shipped prompt entries (design §4.2), compiled in and
/// self-contained. Bodies are real content at approximately the
/// design's token targets (~4,000 / ~30,000 / ~3,000 / ~1,500 /
/// ~4,000 tokens); targets are targets — the runner measures the
/// true count per model at run time.
pub fn shipped_prompts() -> Vec<PromptEntry> {
    vec![
        PromptEntry::new(
            "coder-standard",
            "Coder — edit and continuation",
            "coder",
            CODER_STANDARD_BODY.to_string(),
            PromptOrigin::Shipped,
            SHIPPED_PROMPT_VERSION,
            0,
        ),
        PromptEntry::new(
            "coder-long",
            "Coder — long context",
            "coder",
            CODER_LONG_BODY.to_string(),
            PromptOrigin::Shipped,
            SHIPPED_PROMPT_VERSION,
            0,
        ),
        PromptEntry::new(
            "planner-standard",
            "Planner — constrained plan",
            "planner",
            PLANNER_STANDARD_BODY.to_string(),
            PromptOrigin::Shipped,
            SHIPPED_PROMPT_VERSION,
            0,
        ),
        PromptEntry::new(
            "tool-standard",
            "Tool runner — function calls",
            "tool-runner",
            TOOL_STANDARD_BODY.to_string(),
            PromptOrigin::Shipped,
            SHIPPED_PROMPT_VERSION,
            0,
        ),
        PromptEntry::new(
            "general-standard",
            "General — prose and questions",
            "general",
            GENERAL_STANDARD_BODY.to_string(),
            PromptOrigin::Shipped,
            SHIPPED_PROMPT_VERSION,
            0,
        ),
    ]
}

/// The shipped bodies, for depth padding (§2.1): the runner
/// concatenates distinct blocks until the target depth is reached —
/// never the same block twice back-to-back, because repetition
/// would falsify n-gram acceptance in spec arms measured at depth.
pub fn padding_blocks() -> Vec<&'static str> {
    vec![
        CODER_STANDARD_BODY,
        CODER_LONG_BODY,
        PLANNER_STANDARD_BODY,
        TOOL_STANDARD_BODY,
        GENERAL_STANDARD_BODY,
    ]
}

/// The prompt library store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PromptLibrary {
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<PromptEntry>,
}

impl Default for PromptLibrary {
    fn default() -> Self {
        PromptLibrary {
            version: PROMPT_LIBRARY_FORMAT_VERSION,
            entries: Vec::new(),
        }
    }
}

impl PromptLibrary {
    /// Load the library at `path`. A missing file is an empty
    /// library (first run, before materialization); a present but
    /// unparseable file is an error, so a corrupt store is surfaced
    /// rather than silently emptied.
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut lib: PromptLibrary = serde_json::from_str(&text)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                lib.version = PROMPT_LIBRARY_FORMAT_VERSION;
                Ok(lib)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(PromptLibrary::default()),
            Err(e) => Err(e),
        }
    }

    /// Save the library at `path` as pretty JSON, creating parent
    /// directories when needed.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, text)
    }

    /// Insert any missing shipped entries and refresh shipped
    /// entries whose version is older than the compiled-in shipped
    /// version. Entries with origin `user` or `generated` are never
    /// touched — an operator edit flips a shipped entry's origin to
    /// `user`, which is what protects the edit here. Returns whether
    /// the library changed.
    pub fn materialize_shipped(&mut self) -> bool {
        let mut changed = false;
        for shipped in shipped_prompts() {
            match self.entries.iter_mut().find(|e| e.id == shipped.id) {
                None => {
                    self.entries.push(shipped);
                    changed = true;
                }
                Some(existing) => {
                    if existing.origin == PromptOrigin::Shipped
                        && existing.version < shipped.version
                    {
                        *existing = shipped;
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// Insert an entry, or replace the entry with the same id.
    pub fn upsert(&mut self, entry: PromptEntry) {
        if let Some(existing) = self.entries.iter_mut().find(|e| e.id == entry.id) {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }

    /// Remove the entry with `id`; returns whether one was removed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        self.entries.len() != before
    }

    /// Look up an entry by id.
    pub fn get(&self, id: &str) -> Option<&PromptEntry> {
        self.entries.iter().find(|e| e.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_prompts_cover_the_five_purposes() {
        let shipped = shipped_prompts();
        assert_eq!(shipped.len(), 5);
        let purposes: Vec<(&str, &str)> = shipped
            .iter()
            .map(|e| (e.id.as_str(), e.purpose.as_str()))
            .collect();
        assert_eq!(
            purposes,
            vec![
                ("coder-standard", "coder"),
                ("coder-long", "coder"),
                ("planner-standard", "planner"),
                ("tool-standard", "tool-runner"),
                ("general-standard", "general"),
            ]
        );
        for e in &shipped {
            assert_eq!(e.origin, PromptOrigin::Shipped);
            assert_eq!(e.sha256, prompt_sha256(&e.body), "hash of {}", e.id);
        }
    }

    #[test]
    fn shipped_bodies_are_non_trivial() {
        let by_id = |id: &str| {
            shipped_prompts()
                .into_iter()
                .find(|e| e.id == id)
                .expect("shipped entry")
        };
        assert!(
            by_id("coder-standard").body.chars().count() > 8_000,
            "coder-standard body is {} chars",
            by_id("coder-standard").body.chars().count()
        );
        assert!(
            by_id("coder-long").body.chars().count() > 60_000,
            "coder-long body is {} chars",
            by_id("coder-long").body.chars().count()
        );
        for id in ["planner-standard", "tool-standard", "general-standard"] {
            assert!(by_id(id).body.chars().count() > 2_000, "{id} too short");
        }
        // Padding blocks are the distinct shipped bodies.
        let blocks = padding_blocks();
        assert_eq!(blocks.len(), 5);
        for (i, a) in blocks.iter().enumerate() {
            for b in blocks.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn materialize_inserts_then_is_idempotent() {
        let mut lib = PromptLibrary::default();
        assert!(lib.materialize_shipped());
        assert_eq!(lib.entries.len(), 5);
        assert!(!lib.materialize_shipped());
        assert_eq!(lib.entries.len(), 5);
    }

    #[test]
    fn materialize_refreshes_stale_shipped_but_preserves_user_copy() {
        let mut lib = PromptLibrary::default();
        lib.materialize_shipped();
        // Simulate an entry from an older app version: shipped
        // origin, older version, stale text.
        let stale = PromptEntry::new(
            "coder-standard",
            "Old name",
            "coder",
            "stale body".to_string(),
            PromptOrigin::Shipped,
            0,
            0,
        );
        lib.upsert(stale);
        assert!(lib.materialize_shipped());
        let refreshed = lib.get("coder-standard").expect("entry present");
        assert_eq!(refreshed.version, SHIPPED_PROMPT_VERSION);
        assert!(refreshed.body.chars().count() > 8_000);

        // An operator-edited copy: the editor flips origin to User.
        // Even at an older version it must survive materialization.
        let edited = PromptEntry::new(
            "coder-standard",
            "My coder prompt",
            "coder",
            "operator text".to_string(),
            PromptOrigin::User,
            0,
            42,
        );
        lib.upsert(edited);
        assert!(!lib.materialize_shipped());
        let kept = lib.get("coder-standard").expect("entry present");
        assert_eq!(kept.body, "operator text");
        assert_eq!(kept.origin, PromptOrigin::User);
    }

    #[test]
    fn upsert_and_remove_by_id() {
        let mut lib = PromptLibrary::default();
        let entry = PromptEntry::new(
            "mine",
            "Mine",
            "coder",
            "body".to_string(),
            PromptOrigin::User,
            1,
            1,
        );
        lib.upsert(entry.clone());
        lib.upsert(entry);
        assert_eq!(lib.entries.len(), 1);
        assert!(lib.remove("mine"));
        assert!(!lib.remove("mine"));
        assert!(lib.entries.is_empty());
    }

    #[test]
    fn load_missing_file_is_empty_and_roundtrip_saves() {
        let dir = std::env::temp_dir().join(format!(
            "manager-bench-prompts-test-{}",
            std::process::id()
        ));
        let path = dir.join("bench-prompts.json");
        let lib = PromptLibrary::load(&path).expect("missing file loads empty");
        assert!(lib.entries.is_empty());
        let mut lib = PromptLibrary::default();
        lib.materialize_shipped();
        lib.save(&path).expect("save");
        let back = PromptLibrary::load(&path).expect("load saved");
        assert_eq!(back, lib);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
