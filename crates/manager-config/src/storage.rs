//! Tiered model storage: configuration and the model-path resolver
//! (docs/tiered-model-storage.md).
//!
//! A model's *logical* path — what config stores, `models\X.gguf`
//! relative to the data dir — never changes when its file moves between
//! tiers. The hot tier is always `<data_dir>\models`; each configured cold
//! tier is a directory that mirrors it (`models\X.gguf` lives at
//! `<cold>\X.gguf`). [`resolve_model_path`] finds the file: hot first,
//! then each cold tier in configured order.
//!
//! The resolver lives here, in the shared leaf crate, because both the
//! app and the setup wizard resolve model files. It takes an `exists`
//! probe so the wizard can route it through its injectable `SystemOps`.

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// The directory name every tierable logical path starts with.
pub const MODELS_DIR: &str = "models";

/// Suffix of a tier copy in progress (`X.gguf.partial`). Never resolved.
pub const TIER_PARTIAL_SUFFIX: &str = ".partial";

/// `model_storage` section of [`AppConfig`](crate::AppConfig). Every
/// field is serde-defaulted: a configuration written before the section
/// existed loads with no cold tiers, which makes the resolver answer
/// "hot" for every model — exactly the pre-feature behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelStorageConfig {
    /// Cold tiers, probed in order after the hot tier.
    #[serde(default)]
    pub cold_tiers: Vec<ColdTier>,
    /// Cap on tierable GGUF bytes in the hot tier (GiB); `None` = no cap.
    #[serde(default)]
    pub hot_budget_gib: Option<u64>,
    /// Keep at least this much free on the hot volume (GiB).
    #[serde(default = "default_hot_min_free_gib")]
    pub hot_min_free_gib: u64,
    /// Promote a cold model after this many starts …
    #[serde(default = "default_promote_after_starts")]
    pub promote_after_starts: u32,
    /// … within this many days.
    #[serde(default = "default_promote_window_days")]
    pub promote_window_days: u32,
    /// Demote an unpinned hot model not started for this many days;
    /// `None` (JSON `null`) = never demote for idleness.
    #[serde(default = "default_demote_after_idle_days")]
    pub demote_after_idle_days: Option<u32>,
    /// Operator pins: model ids or logical paths kept hot.
    #[serde(default)]
    pub keep_hot: Vec<String>,
    /// Where new downloads land.
    #[serde(default)]
    pub download_tier: DownloadTier,
    /// Copy throttle in MiB/s; 0 = unthrottled.
    #[serde(default = "default_copy_rate_mib_s")]
    pub copy_rate_mib_s: u32,
    /// Policy-driven moves on/off. Manual Promote/Demote always work.
    #[serde(default = "default_true")]
    pub auto: bool,
}

/// One cold tier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColdTier {
    /// Absolute directory that mirrors `<data_dir>\models`.
    pub path: PathBuf,
    /// Operator label (e.g. the volume label). Display only.
    #[serde(default)]
    pub label: String,
}

/// Where new model downloads land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadTier {
    #[default]
    Hot,
    /// The first cold tier (falls back to hot when none is configured
    /// or it is unavailable).
    Cold,
}

fn default_hot_min_free_gib() -> u64 {
    100
}
fn default_promote_after_starts() -> u32 {
    2
}
fn default_promote_window_days() -> u32 {
    14
}
fn default_demote_after_idle_days() -> Option<u32> {
    Some(30)
}
fn default_copy_rate_mib_s() -> u32 {
    256
}
fn default_true() -> bool {
    true
}

impl Default for ModelStorageConfig {
    fn default() -> Self {
        Self {
            cold_tiers: Vec::new(),
            hot_budget_gib: None,
            hot_min_free_gib: default_hot_min_free_gib(),
            promote_after_starts: default_promote_after_starts(),
            promote_window_days: default_promote_window_days(),
            demote_after_idle_days: default_demote_after_idle_days(),
            keep_hot: Vec::new(),
            download_tier: DownloadTier::Hot,
            copy_rate_mib_s: default_copy_rate_mib_s(),
            auto: true,
        }
    }
}

/// A storage tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tier {
    Hot,
    /// Index into [`ModelStorageConfig::cold_tiers`].
    Cold(usize),
}

impl Tier {
    /// Wire form: `"hot"` / `"cold"` (first cold tier) / `"cold:N"`.
    pub fn as_wire(&self) -> String {
        match self {
            Tier::Hot => "hot".to_string(),
            Tier::Cold(0) => "cold".to_string(),
            Tier::Cold(i) => format!("cold:{i}"),
        }
    }

    /// Parse the wire form produced by [`Tier::as_wire`].
    pub fn parse_wire(s: &str) -> Option<Tier> {
        match s {
            "hot" => Some(Tier::Hot),
            "cold" => Some(Tier::Cold(0)),
            _ => s
                .strip_prefix("cold:")
                .and_then(|n| n.parse().ok())
                .map(Tier::Cold),
        }
    }
}

/// Where a model's file was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Residency {
    /// Found in this tier (hot wins when several hold it).
    In(Tier),
    /// Not found in any available tier.
    Missing,
    /// The logical path is not tierable (absolute, or not under
    /// `models\`); resolved exactly as before the feature.
    NotTierable,
}

/// The resolver's answer for one logical path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    /// The file to open. For a missing tierable file, the hot path
    /// (the pre-feature path: download destination, error messages).
    pub path: PathBuf,
    pub residency: Residency,
    /// The hot-tier path (`<data_dir>\models\X.gguf`), identical to the
    /// pre-feature `resolve_under_data_dir` result.
    pub hot_path: PathBuf,
    /// Every tier that holds the file, hot first.
    pub present_in: Vec<Tier>,
    /// True when some configured cold tier's root is missing (drive
    /// absent): a missing file may be on it.
    pub cold_unavailable: bool,
}

impl ResolvedModel {
    pub fn tier(&self) -> Option<Tier> {
        match self.residency {
            Residency::In(t) => Some(t),
            _ => None,
        }
    }
}

/// The pre-feature resolution: relative paths join the data dir,
/// absolute paths pass through.
pub fn resolve_under(data_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        data_dir.join(path)
    }
}

/// The tier-relative part of a logical path: `models\X.gguf` → `X.gguf`
/// (`models` compared case-insensitively). `None` when not tierable.
pub fn tier_relative(logical: &Path) -> Option<PathBuf> {
    if logical.is_absolute() || logical.has_root() {
        return None;
    }
    let mut comps = logical.components();
    match comps.next() {
        Some(Component::Normal(first))
            if first.to_string_lossy().eq_ignore_ascii_case(MODELS_DIR) => {}
        _ => return None,
    }
    let rest: PathBuf = comps.collect();
    let safe = !rest.as_os_str().is_empty()
        && rest.components().all(|c| matches!(c, Component::Normal(_)));
    safe.then_some(rest)
}

/// The logical path for a tier-relative name: `X.gguf` → `models\X.gguf`.
pub fn logical_from_relative(rel: &Path) -> PathBuf {
    Path::new(MODELS_DIR).join(rel)
}

/// Root directory of a tier.
pub fn tier_root(data_dir: &Path, storage: &ModelStorageConfig, tier: Tier) -> Option<PathBuf> {
    match tier {
        Tier::Hot => Some(data_dir.join(MODELS_DIR)),
        Tier::Cold(i) => storage.cold_tiers.get(i).map(|c| c.path.clone()),
    }
}

/// Every configured tier, hot first.
pub fn all_tiers(storage: &ModelStorageConfig) -> Vec<Tier> {
    std::iter::once(Tier::Hot)
        .chain((0..storage.cold_tiers.len()).map(Tier::Cold))
        .collect()
}

/// The file path of a tier-relative name in a tier.
pub fn tier_file(
    data_dir: &Path,
    storage: &ModelStorageConfig,
    tier: Tier,
    rel: &Path,
) -> Option<PathBuf> {
    tier_root(data_dir, storage, tier).map(|r| r.join(rel))
}

/// The in-progress copy path for a destination file.
pub fn tier_partial_path(dest: &Path) -> PathBuf {
    let mut os = dest.as_os_str().to_owned();
    os.push(TIER_PARTIAL_SUFFIX);
    PathBuf::from(os)
}

/// Resolve a logical model path against the tiers with explicit probes:
/// `exists(file)` answers whether a regular file exists, `dir_exists`
/// whether a tier root is reachable.
pub fn resolve_model_path_with(
    data_dir: &Path,
    storage: &ModelStorageConfig,
    logical: &Path,
    exists: &dyn Fn(&Path) -> bool,
    dir_exists: &dyn Fn(&Path) -> bool,
) -> ResolvedModel {
    let hot_path = resolve_under(data_dir, logical);
    let Some(rel) = tier_relative(logical) else {
        return ResolvedModel {
            path: hot_path.clone(),
            residency: Residency::NotTierable,
            hot_path,
            present_in: Vec::new(),
            cold_unavailable: false,
        };
    };
    let mut present_in = Vec::new();
    if exists(&hot_path) {
        present_in.push(Tier::Hot);
    }
    let mut cold_unavailable = false;
    let mut first_cold: Option<PathBuf> = None;
    for (i, cold) in storage.cold_tiers.iter().enumerate() {
        if !dir_exists(&cold.path) {
            cold_unavailable = true;
            continue;
        }
        let candidate = cold.path.join(&rel);
        if exists(&candidate) {
            present_in.push(Tier::Cold(i));
            if first_cold.is_none() {
                first_cold = Some(candidate);
            }
        }
    }
    let (path, residency) = match present_in.first() {
        Some(Tier::Hot) => (hot_path.clone(), Residency::In(Tier::Hot)),
        Some(t @ Tier::Cold(_)) => (
            first_cold.expect("a cold hit records its path"),
            Residency::In(*t),
        ),
        None => (hot_path.clone(), Residency::Missing),
    };
    ResolvedModel {
        path,
        residency,
        hot_path,
        present_in,
        cold_unavailable,
    }
}

/// [`resolve_model_path_with`] against the real filesystem.
pub fn resolve_model_path(
    data_dir: &Path,
    storage: &ModelStorageConfig,
    logical: &Path,
) -> ResolvedModel {
    resolve_model_path_with(
        data_dir,
        storage,
        logical,
        &|p| std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false),
        &|p| std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false),
    )
}

/// Case- and separator-insensitive comparison key for a path (Windows
/// paths are case-insensitive; config paths mix `/` and `\`).
pub fn path_key(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").to_lowercase()
}

/// Whether `keep_hot` pins this file: an entry equal to the model id or
/// to the logical path (path compared via [`path_key`]).
pub fn keep_hot_matches(
    storage: &ModelStorageConfig,
    model_id: Option<&str>,
    logical: &Path,
) -> bool {
    let key = path_key(logical);
    storage.keep_hot.iter().any(|e| {
        let e = e.trim();
        model_id.map(|id| id == e).unwrap_or(false) || path_key(Path::new(e)) == key
    })
}

/// Structural validation of the section — no disk probes (a missing
/// cold drive must never stop the configuration loading). Existence and
/// same-volume checks belong to the operation that edits the section.
pub fn validate_model_storage(
    storage: &ModelStorageConfig,
    data_dir: &Path,
    errors: &mut Vec<String>,
) {
    let data_key = path_key(data_dir);
    let mut seen = std::collections::HashSet::new();
    for (i, cold) in storage.cold_tiers.iter().enumerate() {
        let p = &cold.path;
        if !p.is_absolute() {
            errors.push(format!(
                "model_storage.cold_tiers[{i}].path must be absolute, got {}",
                p.display()
            ));
            continue;
        }
        let key = path_key(p);
        let key_slash = format!("{}\\", key.trim_end_matches('\\'));
        let data_slash = format!("{}\\", data_key.trim_end_matches('\\'));
        if key_slash.starts_with(&data_slash) || data_slash.starts_with(&key_slash) {
            errors.push(format!(
                "model_storage.cold_tiers[{i}].path {} must not overlap the data directory",
                p.display()
            ));
        }
        if !seen.insert(key.trim_end_matches('\\').to_string()) {
            errors.push(format!(
                "model_storage.cold_tiers[{i}].path {} is listed twice",
                p.display()
            ));
        }
    }
    if storage.promote_after_starts == 0 {
        errors.push("model_storage.promote_after_starts must be >= 1".to_string());
    }
    if storage.promote_window_days == 0 {
        errors.push("model_storage.promote_window_days must be >= 1".to_string());
    }
    if storage.demote_after_idle_days == Some(0) {
        errors.push("model_storage.demote_after_idle_days must be >= 1 or null".to_string());
    }
    if storage.hot_budget_gib == Some(0) {
        errors.push("model_storage.hot_budget_gib must be >= 1 or null".to_string());
    }
    if storage.keep_hot.iter().any(|e| e.trim().is_empty()) {
        errors.push("model_storage.keep_hot entries must not be empty".to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn storage(cold: &[&str]) -> ModelStorageConfig {
        ModelStorageConfig {
            cold_tiers: cold
                .iter()
                .map(|p| ColdTier {
                    path: PathBuf::from(p),
                    label: String::new(),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn probe<'a>(files: &'a HashSet<PathBuf>) -> impl Fn(&Path) -> bool + 'a {
        move |p| files.contains(p)
    }

    #[cfg(windows)]
    const DATA: &str = r"C:\data";
    #[cfg(not(windows))]
    const DATA: &str = "/data";
    #[cfg(windows)]
    const COLD: &str = r"E:\cold";
    #[cfg(not(windows))]
    const COLD: &str = "/cold";
    #[cfg(windows)]
    const COLD2: &str = r"G:\cold2";
    #[cfg(not(windows))]
    const COLD2: &str = "/cold2";

    fn hot(name: &str) -> PathBuf {
        Path::new(DATA).join("models").join(name)
    }
    fn cold(name: &str) -> PathBuf {
        Path::new(COLD).join(name)
    }

    #[test]
    fn tier_relative_accepts_only_models_paths() {
        assert_eq!(
            tier_relative(Path::new("models/X.gguf")),
            Some(PathBuf::from("X.gguf"))
        );
        assert_eq!(
            tier_relative(Path::new("Models/sub/X.gguf")),
            Some(PathBuf::from("sub").join("X.gguf"))
        );
        assert_eq!(tier_relative(Path::new("other/X.gguf")), None);
        assert_eq!(tier_relative(Path::new("models")), None);
        assert_eq!(tier_relative(Path::new("models/../X.gguf")), None);
        assert_eq!(
            tier_relative(&hot("X.gguf")),
            None,
            "absolute is not tierable"
        );
    }

    #[test]
    fn no_cold_tiers_is_the_pre_feature_resolution() {
        let files: HashSet<PathBuf> = HashSet::new();
        let r = resolve_model_path_with(
            Path::new(DATA),
            &ModelStorageConfig::default(),
            Path::new("models/X.gguf"),
            &probe(&files),
            &|_| true,
        );
        assert_eq!(r.path, Path::new(DATA).join("models/X.gguf"));
        assert_eq!(
            r.hot_path,
            resolve_under(Path::new(DATA), Path::new("models/X.gguf"))
        );
        assert_eq!(r.residency, Residency::Missing);
        assert!(!r.cold_unavailable);
    }

    #[test]
    fn hot_hit_cold_hit_and_both() {
        let s = storage(&[COLD]);
        let logical = Path::new("models").join("X.gguf");
        let mut files = HashSet::new();
        files.insert(cold("X.gguf"));
        let r = resolve_model_path_with(Path::new(DATA), &s, &logical, &probe(&files), &|_| true);
        assert_eq!(r.residency, Residency::In(Tier::Cold(0)));
        assert_eq!(r.path, cold("X.gguf"));
        assert_eq!(r.present_in, vec![Tier::Cold(0)]);

        files.insert(hot("X.gguf"));
        let r = resolve_model_path_with(Path::new(DATA), &s, &logical, &probe(&files), &|_| true);
        assert_eq!(r.residency, Residency::In(Tier::Hot), "hot wins");
        assert_eq!(r.path, hot("X.gguf"));
        assert_eq!(r.present_in, vec![Tier::Hot, Tier::Cold(0)]);
    }

    #[test]
    fn cold_tiers_probe_in_order_and_unavailable_is_reported() {
        let s = storage(&[COLD, COLD2]);
        let logical = Path::new("models").join("X.gguf");
        let mut files = HashSet::new();
        files.insert(Path::new(COLD2).join("X.gguf"));
        let r = resolve_model_path_with(Path::new(DATA), &s, &logical, &probe(&files), &|d| {
            d != Path::new(COLD)
        });
        assert_eq!(r.residency, Residency::In(Tier::Cold(1)));
        assert!(r.cold_unavailable);

        let none: HashSet<PathBuf> = HashSet::new();
        let r = resolve_model_path_with(Path::new(DATA), &s, &logical, &probe(&none), &|_| false);
        assert_eq!(r.residency, Residency::Missing);
        assert_eq!(r.path, hot("X.gguf"));
        assert!(r.cold_unavailable);
    }

    #[test]
    fn partial_copies_never_resolve() {
        let s = storage(&[COLD]);
        let mut files = HashSet::new();
        files.insert(tier_partial_path(&hot("X.gguf")));
        files.insert(cold("X.gguf"));
        let r = resolve_model_path_with(
            Path::new(DATA),
            &s,
            &Path::new("models").join("X.gguf"),
            &probe(&files),
            &|_| true,
        );
        assert_eq!(r.residency, Residency::In(Tier::Cold(0)));
    }

    #[test]
    fn non_tierable_paths_pass_through() {
        let s = storage(&[COLD]);
        let abs = hot("X.gguf");
        let files: HashSet<PathBuf> = HashSet::new();
        let r = resolve_model_path_with(Path::new(DATA), &s, &abs, &probe(&files), &|_| true);
        assert_eq!(r.residency, Residency::NotTierable);
        assert_eq!(r.path, abs);
        let r = resolve_model_path_with(
            Path::new(DATA),
            &s,
            Path::new("elsewhere/X.gguf"),
            &probe(&files),
            &|_| true,
        );
        assert_eq!(r.path, Path::new(DATA).join("elsewhere/X.gguf"));
    }

    #[test]
    fn section_absent_deserializes_to_defaults() {
        let s: ModelStorageConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(s, ModelStorageConfig::default());
        assert_eq!(s.demote_after_idle_days, Some(30));
        let s: ModelStorageConfig =
            serde_json::from_str(r#"{"demote_after_idle_days": null, "download_tier": "cold"}"#)
                .unwrap();
        assert_eq!(s.demote_after_idle_days, None);
        assert_eq!(s.download_tier, DownloadTier::Cold);
        let back: ModelStorageConfig =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s, "null idle round-trips as never");
    }

    #[test]
    fn config_written_before_the_section_loads_and_validates() {
        let mut raw = serde_json::to_value(crate::AppConfig::default_config()).unwrap();
        raw.as_object_mut().unwrap().remove("model_storage");
        let cfg: crate::AppConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(cfg.model_storage, ModelStorageConfig::default());
        crate::validate(&cfg).expect("an old config stays valid");
        // And the defaulted section round-trips through save's JSON.
        let again: crate::AppConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(again.model_storage, cfg.model_storage);
    }

    #[test]
    fn structural_validation() {
        let mut errors = Vec::new();
        validate_model_storage(&storage(&[COLD]), Path::new(DATA), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");

        let mut s = storage(&["relative/dir", COLD, COLD]);
        s.promote_after_starts = 0;
        s.demote_after_idle_days = Some(0);
        s.keep_hot = vec![" ".into()];
        let mut errors = Vec::new();
        validate_model_storage(&s, Path::new(DATA), &mut errors);
        assert_eq!(errors.len(), 5, "{errors:?}");

        let inside = Path::new(DATA)
            .join("models")
            .to_string_lossy()
            .into_owned();
        let mut errors = Vec::new();
        validate_model_storage(&storage(&[&inside]), Path::new(DATA), &mut errors);
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn keep_hot_matches_id_or_logical_path() {
        let mut s = storage(&[]);
        s.keep_hot = vec!["draft".into(), r"models\Orphan.gguf".into()];
        assert!(keep_hot_matches(
            &s,
            Some("draft"),
            Path::new("models/D.gguf")
        ));
        assert!(keep_hot_matches(&s, None, Path::new("models/orphan.gguf")));
        assert!(!keep_hot_matches(
            &s,
            Some("other"),
            Path::new("models/D.gguf")
        ));
    }

    #[test]
    fn tier_wire_round_trips() {
        for t in [Tier::Hot, Tier::Cold(0), Tier::Cold(3)] {
            assert_eq!(Tier::parse_wire(&t.as_wire()), Some(t));
        }
        assert_eq!(Tier::parse_wire("warm"), None);
    }
}
