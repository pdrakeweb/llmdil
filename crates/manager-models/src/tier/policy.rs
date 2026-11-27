//! The promotion/demotion policy: a pure function from the facts about
//! every tierable file to the moves the mover should run, demotions
//! first (they free hot space promotions may need). The app re-plans
//! after every move, so only the first entry is acted on at a time;
//! the rest is shown as "planned".

use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};

use super::{COPY_HEADROOM_BYTES, GIB};
use manager_config::ModelStorageConfig;

/// Why a file must stay hot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinReason {
    /// Referenced by this enabled backend's `model_file`.
    Backend(String),
    /// Named by path in this enabled backend's raw flags.
    RawFlags(String),
    /// Listed in `model_storage.keep_hot`.
    KeepHot,
}

impl PinReason {
    pub fn describe(&self) -> String {
        match self {
            PinReason::Backend(b) => format!("used by enabled backend {b}"),
            PinReason::RawFlags(b) => format!("named in enabled backend {b}'s flags"),
            PinReason::KeepHot => "keep-hot pin".to_string(),
        }
    }
}

/// What the policy needs to know about one tierable file.
#[derive(Debug, Clone)]
pub struct FileFacts {
    pub logical: PathBuf,
    pub model_id: Option<String>,
    pub size: u64,
    /// A copy is in the hot tier.
    pub hot: bool,
    /// A copy is in the (first available) cold tier.
    pub cold: bool,
    /// The cold copy is a valid archive (demotion is an instant delete).
    pub archived: bool,
    pub pin: Option<PinReason>,
    /// Open by a running process (never demote).
    pub in_use: bool,
    /// A download, verification or move is in flight for it.
    pub busy: bool,
    /// LRU key: last start, else when first seen.
    pub last_used: Option<DateTime<Utc>>,
    /// Starts within the promotion window.
    pub recent_starts: u32,
}

#[derive(Debug, Clone)]
pub struct PlanInputs<'a> {
    pub files: &'a [FileFacts],
    pub storage: &'a ModelStorageConfig,
    pub now: DateTime<Utc>,
    /// Free bytes on the hot volume; `None` = unknown (reserve rules off).
    pub hot_free_bytes: Option<u64>,
    /// Free bytes on the cold volume; `None` = unknown.
    pub cold_free_bytes: Option<u64>,
    /// The first cold tier is configured and reachable.
    pub cold_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveKind {
    Promote,
    Demote,
}

impl MoveKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MoveKind::Promote => "promote",
            MoveKind::Demote => "demote",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMove {
    pub logical: PathBuf,
    pub kind: MoveKind,
    pub reason: String,
}

/// A move the policy wants but cannot make (surfaced as an alert).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedMove {
    pub logical: PathBuf,
    pub kind: MoveKind,
    pub reason: String,
}

/// Plan the policy moves. Empty when `auto` is off, no cold tier is
/// configured, or the cold tier is unavailable.
pub fn plan(inputs: &PlanInputs<'_>) -> (Vec<PlannedMove>, Vec<SkippedMove>) {
    let s = inputs.storage;
    let mut demotes: Vec<PlannedMove> = Vec::new();
    let mut promotes: Vec<PlannedMove> = Vec::new();
    let mut skipped: Vec<SkippedMove> = Vec::new();
    if !s.auto || s.cold_tiers.is_empty() || !inputs.cold_available {
        return (demotes, skipped);
    }
    let files = inputs.files;
    let reserve = s.hot_min_free_gib.saturating_mul(GIB);
    let budget = s.hot_budget_gib.map(|g| g.saturating_mul(GIB));
    let n = s.promote_after_starts.max(1);

    let mut planned = vec![false; files.len()];
    let mut hot_usage: u64 = files.iter().filter(|f| f.hot).map(|f| f.size).sum();
    let mut hot_free = inputs.hot_free_bytes;
    let mut cold_free = inputs.cold_free_bytes;

    let demotable = |f: &FileFacts| f.hot && f.pin.is_none() && !f.in_use && !f.busy;
    // Space a demotion needs on cold: none when the archive is intact.
    let cold_need = |f: &FileFacts| {
        if f.archived {
            0
        } else {
            f.size + COPY_HEADROOM_BYTES
        }
    };

    // Apply a demotion to the projections; false when cold has no room.
    let take = |i: usize,
                reason: String,
                planned: &mut Vec<bool>,
                hot_usage: &mut u64,
                hot_free: &mut Option<u64>,
                cold_free: &mut Option<u64>,
                demotes: &mut Vec<PlannedMove>,
                skipped: &mut Vec<SkippedMove>|
     -> bool {
        let f = &files[i];
        let need = cold_need(f);
        if let Some(cf) = *cold_free {
            if need > cf {
                skipped.push(SkippedMove {
                    logical: f.logical.clone(),
                    kind: MoveKind::Demote,
                    reason: "cold tier full".to_string(),
                });
                planned[i] = true;
                return false;
            }
            *cold_free = Some(cf - if f.archived { 0 } else { f.size });
        }
        planned[i] = true;
        *hot_usage = hot_usage.saturating_sub(f.size);
        if let Some(hf) = hot_free.as_mut() {
            *hf += f.size;
        }
        demotes.push(PlannedMove {
            logical: f.logical.clone(),
            kind: MoveKind::Demote,
            reason,
        });
        true
    };

    // 1. Idle demotions.
    if let Some(days) = s.demote_after_idle_days {
        let cutoff = inputs.now - Duration::days(i64::from(days));
        for (i, f) in files.iter().enumerate() {
            let idle = f.last_used.map(|t| t < cutoff).unwrap_or(false);
            if demotable(f) && idle && f.recent_starts < n {
                take(
                    i,
                    format!("idle {days}+ days"),
                    &mut planned,
                    &mut hot_usage,
                    &mut hot_free,
                    &mut cold_free,
                    &mut demotes,
                    &mut skipped,
                );
            }
        }
    }

    // LRU order over demotable files (unknown last use = oldest).
    let mut lru: Vec<usize> = (0..files.len()).filter(|&i| demotable(&files[i])).collect();
    lru.sort_by_key(|&i| files[i].last_used);

    // 2. Pressure demotions.
    let over = |usage: u64, free: Option<u64>| {
        budget.map(|b| usage > b).unwrap_or(false) || free.map(|f| f < reserve).unwrap_or(false)
    };
    for &i in &lru {
        if !over(hot_usage, hot_free) {
            break;
        }
        if planned[i] {
            continue;
        }
        let reason = if budget.map(|b| hot_usage > b).unwrap_or(false) {
            "hot tier over budget"
        } else {
            "hot free space below reserve"
        };
        take(
            i,
            reason.to_string(),
            &mut planned,
            &mut hot_usage,
            &mut hot_free,
            &mut cold_free,
            &mut demotes,
            &mut skipped,
        );
    }

    // 3. Promotions: pinned first, then the most-started.
    let mut wanted: Vec<(usize, String)> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.hot && f.cold && !f.busy)
        .filter_map(|(i, f)| match &f.pin {
            Some(p) => Some((i, p.describe())),
            None if f.recent_starts >= n => Some((
                i,
                format!(
                    "{} starts in {} days",
                    f.recent_starts, s.promote_window_days
                ),
            )),
            None => None,
        })
        .collect();
    wanted.sort_by_key(|(i, _)| {
        (
            files[*i].pin.is_none(),
            std::cmp::Reverse(files[*i].recent_starts),
        )
    });
    for (i, reason) in wanted {
        let f = &files[i];
        let fits = |usage: u64, free: Option<u64>| {
            budget.map(|b| usage + f.size <= b).unwrap_or(true)
                && free
                    .map(|fr| fr >= f.size + COPY_HEADROOM_BYTES && fr - f.size >= reserve)
                    .unwrap_or(true)
        };
        // Try evicting LRU files that are not themselves hot by use.
        let mut trial_usage = hot_usage;
        let mut trial_free = hot_free;
        let mut evict: Vec<usize> = Vec::new();
        for &j in &lru {
            if fits(trial_usage, trial_free) {
                break;
            }
            if planned[j] || files[j].recent_starts >= n {
                continue;
            }
            trial_usage = trial_usage.saturating_sub(files[j].size);
            trial_free = trial_free.map(|x| x + files[j].size);
            evict.push(j);
        }
        if !fits(trial_usage, trial_free) {
            skipped.push(SkippedMove {
                logical: f.logical.clone(),
                kind: MoveKind::Promote,
                reason: "hot tier full".to_string(),
            });
            continue;
        }
        let mut ok = true;
        for j in evict {
            ok &= take(
                j,
                format!("making room for {}", f.logical.display()),
                &mut planned,
                &mut hot_usage,
                &mut hot_free,
                &mut cold_free,
                &mut demotes,
                &mut skipped,
            );
        }
        if !ok {
            skipped.push(SkippedMove {
                logical: f.logical.clone(),
                kind: MoveKind::Promote,
                reason: "hot tier full".to_string(),
            });
            continue;
        }
        hot_usage += f.size;
        hot_free = hot_free.map(|x| x.saturating_sub(f.size));
        promotes.push(PlannedMove {
            logical: f.logical.clone(),
            kind: MoveKind::Promote,
            reason,
        });
    }

    demotes.extend(promotes);
    (demotes, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use manager_config::ColdTier;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-26T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn storage() -> ModelStorageConfig {
        ModelStorageConfig {
            cold_tiers: vec![ColdTier {
                path: PathBuf::from(r"E:\llm-models"),
                label: String::new(),
            }],
            hot_min_free_gib: 100,
            ..Default::default()
        }
    }

    fn file(name: &str, gib: u64) -> FileFacts {
        FileFacts {
            logical: PathBuf::from(format!("models/{name}.gguf")),
            model_id: Some(name.to_string()),
            size: gib * GIB,
            hot: true,
            cold: false,
            archived: false,
            pin: None,
            in_use: false,
            busy: false,
            last_used: Some(now()),
            recent_starts: 0,
        }
    }

    fn days_ago(d: i64) -> Option<DateTime<Utc>> {
        Some(now() - Duration::days(d))
    }

    fn run(
        files: &[FileFacts],
        s: &ModelStorageConfig,
        hot_free_gib: Option<u64>,
    ) -> (Vec<PlannedMove>, Vec<SkippedMove>) {
        plan(&PlanInputs {
            files,
            storage: s,
            now: now(),
            hot_free_bytes: hot_free_gib.map(|g| g * GIB),
            cold_free_bytes: Some(1000 * GIB),
            cold_available: true,
        })
    }

    fn names(moves: &[PlannedMove]) -> Vec<(String, MoveKind)> {
        moves
            .iter()
            .map(|m| {
                (
                    m.logical
                        .file_stem()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    m.kind,
                )
            })
            .collect()
    }

    #[test]
    fn nothing_moves_without_auto_cold_tier_or_cold_availability() {
        let mut idle = file("a", 10);
        idle.last_used = days_ago(90);
        let files = vec![idle];
        let mut s = storage();
        s.auto = false;
        assert!(run(&files, &s, Some(400)).0.is_empty());
        assert!(run(&files, &ModelStorageConfig::default(), Some(400))
            .0
            .is_empty());
        let out = plan(&PlanInputs {
            files: &files,
            storage: &storage(),
            now: now(),
            hot_free_bytes: Some(400 * GIB),
            cold_free_bytes: None,
            cold_available: false,
        });
        assert!(out.0.is_empty());
    }

    #[test]
    fn idle_unpinned_files_demote_and_pinned_in_use_busy_never_do() {
        let mut idle = file("idle", 10);
        idle.last_used = days_ago(31);
        let mut fresh = file("fresh", 10);
        fresh.last_used = days_ago(29);
        let mut pinned = file("pinned", 10);
        pinned.last_used = days_ago(90);
        pinned.pin = Some(PinReason::Backend("coder".into()));
        let mut keep = file("keep", 10);
        keep.last_used = days_ago(90);
        keep.pin = Some(PinReason::KeepHot);
        let mut open = file("open", 10);
        open.last_used = days_ago(90);
        open.in_use = true;
        let mut busy = file("busy", 10);
        busy.last_used = days_ago(90);
        busy.busy = true;
        let files = vec![idle, fresh, pinned, keep, open, busy];
        let (moves, skipped) = run(&files, &storage(), Some(400));
        assert_eq!(names(&moves), vec![("idle".into(), MoveKind::Demote)]);
        assert!(skipped.is_empty());
    }

    #[test]
    fn idle_demotion_can_be_turned_off() {
        let mut idle = file("idle", 10);
        idle.last_used = days_ago(365);
        let mut s = storage();
        s.demote_after_idle_days = None;
        assert!(run(&[idle], &s, Some(400)).0.is_empty());
    }

    #[test]
    fn n_starts_in_the_window_promote_and_pins_promote_first() {
        let mut once = file("once", 5);
        once.hot = false;
        once.cold = true;
        once.recent_starts = 1;
        let mut twice = file("twice", 5);
        twice.hot = false;
        twice.cold = true;
        twice.recent_starts = 2;
        let mut pinned = file("pinned", 5);
        pinned.hot = false;
        pinned.cold = true;
        pinned.pin = Some(PinReason::Backend("coder".into()));
        let files = vec![once, twice, pinned];
        let (moves, _) = run(&files, &storage(), Some(400));
        assert_eq!(
            names(&moves),
            vec![
                ("pinned".into(), MoveKind::Promote),
                ("twice".into(), MoveKind::Promote)
            ]
        );
        assert!(moves[0].reason.contains("coder"));
    }

    #[test]
    fn pressure_demotes_lru_until_the_reserve_holds() {
        let mut old = file("old", 20);
        old.last_used = days_ago(20);
        let mut mid = file("mid", 20);
        mid.last_used = days_ago(10);
        let mut new = file("new", 20);
        new.last_used = days_ago(1);
        let files = vec![new, mid, old];
        // 70 GiB free, 100 reserve: needs 30 more → old (20) then mid (20).
        let (moves, _) = run(&files, &storage(), Some(70));
        assert_eq!(
            names(&moves),
            vec![
                ("old".into(), MoveKind::Demote),
                ("mid".into(), MoveKind::Demote)
            ]
        );
        assert!(moves[0].reason.contains("reserve"));
    }

    #[test]
    fn a_budget_caps_hot_bytes() {
        let mut a = file("a", 30);
        a.last_used = days_ago(5);
        let mut b = file("b", 30);
        b.last_used = days_ago(2);
        let mut s = storage();
        s.hot_budget_gib = Some(40);
        let (moves, _) = run(&[a, b], &s, Some(400));
        assert_eq!(names(&moves), vec![("a".into(), MoveKind::Demote)]);
        assert!(moves[0].reason.contains("budget"));
    }

    #[test]
    fn a_promotion_evicts_lru_but_never_a_hot_by_use_file() {
        let mut want = file("want", 30);
        want.hot = false;
        want.cold = true;
        want.pin = Some(PinReason::Backend("coder".into()));
        let mut lru = file("lru", 30);
        lru.last_used = days_ago(5);
        let mut used = file("used", 30);
        used.last_used = days_ago(6);
        used.recent_starts = 3;
        let files = vec![want, lru, used];
        let mut s = storage();
        s.hot_budget_gib = Some(70);
        let (moves, skipped) = run(&files, &s, Some(400));
        assert_eq!(
            names(&moves),
            vec![
                ("lru".into(), MoveKind::Demote),
                ("want".into(), MoveKind::Promote)
            ]
        );
        assert!(skipped.is_empty());
    }

    #[test]
    fn a_promotion_that_cannot_fit_is_skipped_not_forced() {
        let mut want = file("want", 50);
        want.hot = false;
        want.cold = true;
        want.recent_starts = 5;
        let mut pinned = file("pinned", 50);
        pinned.pin = Some(PinReason::KeepHot);
        let files = vec![want, pinned];
        let (moves, skipped) = run(&files, &storage(), Some(120));
        assert!(moves.is_empty(), "{moves:?}");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].kind, MoveKind::Promote);
    }

    #[test]
    fn a_demotion_needing_a_copy_is_skipped_when_cold_is_full_but_archived_ones_are_free() {
        let mut copy = file("copy", 30);
        copy.last_used = days_ago(60);
        let mut archived = file("archived", 30);
        archived.last_used = days_ago(60);
        archived.cold = true;
        archived.archived = true;
        let files = vec![copy, archived];
        let out = plan(&PlanInputs {
            files: &files,
            storage: &storage(),
            now: now(),
            hot_free_bytes: Some(400 * GIB),
            cold_free_bytes: Some(10 * GIB),
            cold_available: true,
        });
        assert_eq!(names(&out.0), vec![("archived".into(), MoveKind::Demote)]);
        assert_eq!(out.1.len(), 1);
        assert_eq!(out.1[0].reason, "cold tier full");
    }
}
