//! Parallel slots in a backend's raw llama-server flags: the
//! `--parallel` / `-np` count of request slots (independent
//! sequences) the server runs.
//!
//! The decode/encode pair mirrors the expert-offload pair in
//! [`super::memory`] and the MTP pair in [`super::mtp`]: state is
//! decoded from the saved raw flags — there is no separate stored
//! setting — and the writer rewrites only the tokens the setting
//! owns, leaving every other flag untouched and in order.
//! Malformed or hand-edited spellings degrade to a safe decoded
//! state, never a panic: an unparseable value reads as unset, and
//! a malformed value token the writer removes its flag for is
//! preserved rather than deleted.
//!
//! The decoder moved here from [`super::mtp`] when tracker #61
//! gave the slot count a flags-form control of its own: the MTP
//! control only reads it (its drafting path needs a single slot,
//! and it surfaces a conflicting count rather than rewriting it),
//! while the Parallel slots field reads and writes it.

/// llama.cpp's own default for `--parallel` when the flag is
/// absent: -1, "auto" (llama-server(1): "-np, --parallel N —
/// number of server slots (default: -1, -1 = auto)"). A backend
/// whose flags write `--parallel -1` has chosen auto explicitly,
/// while a backend with no token at all runs the same auto mode
/// by default — the decoder returns the explicit value, so the
/// two states stay distinguishable (a saved setting vs no
/// setting) even though the server treats them alike.
pub const LLAMA_CPP_PARALLEL_AUTO: i64 = -1;

/// The slot count auto mode initializes on the app's pinned
/// llama.cpp build (b11658), verified from captured server
/// banners: backends started with no `--parallel` token log
/// "load_model: initializing, n_slots = 4, n_ctx_slot = <ctx>,
/// kv_unified = 'true'" (planner and planner-mtp in the
/// 2026-10-18 diagnostics bundle, both with `raw_flags: null`).
/// The flags form names this figure when it presents the unset
/// state, so the operator sees the effective slot count rather
/// than a bare "auto".
pub const LLAMA_CPP_PARALLEL_AUTO_SLOTS: i64 = 4;

/// The Parallel slots control's range. The set op validates
/// against it; a hand-edited value outside it still decodes (the
/// control renders it as hand-set rather than inventing a
/// position). The ceiling is the app's own arithmetic: the
/// context floor is 1,024 tokens and the context ceiling
/// 1,048,576, so past 1,024 slots even minimum-size slots
/// exceed the largest context the app accepts — llama.cpp
/// imposes no practical bound of its own to mirror.
pub const PARALLEL_SLOTS_MIN: u32 = 1;
pub const PARALLEL_SLOTS_MAX: u32 = 1024;

/// The `--parallel` / `-np` slot count in raw flags, when set
/// explicitly: the last occurrence wins (llama.cpp's
/// sequential-parse rule). A value that does not parse, or a
/// trailing flag, reads as unset — decoding never invents a
/// setting the flags do not state. Auto (-1) is an explicit,
/// parseable setting and decodes as one.
pub fn parallel_slots(raw_flags: Option<&str>) -> Option<i64> {
    let raw = raw_flags?;
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let mut slots: Option<i64> = None;
    for (i, t) in tokens.iter().enumerate() {
        if *t == "--parallel" || *t == "-np" {
            if let Some(v) = tokens.get(i + 1) {
                if let Ok(n) = v.parse::<i64>() {
                    slots = Some(n);
                }
            }
        } else if let Some(v) = t
            .strip_prefix("--parallel=")
            .or_else(|| t.strip_prefix("-np="))
        {
            if let Ok(n) = v.parse::<i64>() {
                slots = Some(n);
            }
        }
    }
    slots
}

/// The slot count a backend actually runs with: the explicit
/// `--parallel` value when it is a positive count, otherwise
/// llama.cpp's auto default ([`LLAMA_CPP_PARALLEL_AUTO_SLOTS`]) —
/// an absent token, an explicit auto (-1), and a malformed or
/// non-positive value all land on auto, matching how the server
/// treats them. Consumers that size per-sequence state (the
/// recurrent-state cache term in the memory requirement) need
/// this effective count, not the raw decoded setting. An explicit
/// count past the control's ceiling clamps to it (the ceiling's
/// own arithmetic: past it even minimum-size slots exceed the
/// largest context the app accepts).
pub fn effective_slots(raw_flags: Option<&str>) -> u32 {
    match parallel_slots(raw_flags) {
        Some(n) if n > 0 => (n as u32).min(PARALLEL_SLOTS_MAX),
        _ => LLAMA_CPP_PARALLEL_AUTO_SLOTS as u32,
    }
}

/// Rewrite the parallel tokens in raw llama-server flags,
/// leaving every other flag untouched and in order. All existing
/// `--parallel` / `-np` occurrences (both spellings, including
/// the `=` forms) are removed, then the canonical
/// `--parallel N` is appended for `slots`; `None` removes the
/// tokens without appending, returning the backend to
/// llama.cpp's default (auto). Returns `None` when nothing
/// remains.
pub fn set_parallel_slots(raw_flags: Option<&str>, slots: Option<i64>) -> Option<String> {
    let tokens: Vec<&str> = raw_flags.unwrap_or("").split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t == "--parallel" || t == "-np" {
            // Skip the value token too when it parses as a slot
            // count; a malformed value is left for the main walk
            // (mirrors set_expert_offload).
            let has_value = tokens
                .get(i + 1)
                .map(|v| v.parse::<i64>().is_ok())
                .unwrap_or(false);
            i += if has_value { 2 } else { 1 };
            continue;
        }
        if t.starts_with("--parallel=") || t.starts_with("-np=") {
            i += 1;
            continue;
        }
        out.push(t.to_string());
        i += 1;
    }
    if let Some(n) = slots {
        out.push("--parallel".to_string());
        out.push(n.to_string());
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_slots_decode() {
        assert_eq!(parallel_slots(None), None);
        assert_eq!(parallel_slots(Some("-c 262144")), None);
        assert_eq!(parallel_slots(Some("--parallel 1")), Some(1));
        assert_eq!(parallel_slots(Some("-np 4")), Some(4));
        assert_eq!(parallel_slots(Some("--parallel=2")), Some(2));
        assert_eq!(parallel_slots(Some("-np=3")), Some(3));
        // Auto is an explicit, parseable setting.
        assert_eq!(parallel_slots(Some("--parallel -1")), Some(-1));
        assert_eq!(
            parallel_slots(Some("--parallel -1")),
            Some(LLAMA_CPP_PARALLEL_AUTO)
        );
        // Last wins; malformed reads as unset.
        assert_eq!(
            parallel_slots(Some("--parallel 4 --parallel 1")),
            Some(1)
        );
        assert_eq!(parallel_slots(Some("--parallel abc")), None);
        assert_eq!(parallel_slots(Some("--parallel")), None);
    }

    #[test]
    fn effective_slots_resolves_auto_and_clamps() {
        // Absent, explicit auto, and malformed all run auto (4).
        assert_eq!(effective_slots(None), 4);
        assert_eq!(effective_slots(Some("-ot exps=CPU")), 4);
        assert_eq!(effective_slots(Some("--parallel -1")), 4);
        assert_eq!(effective_slots(Some("--parallel abc")), 4);
        // A positive explicit count is the count.
        assert_eq!(effective_slots(Some("--parallel 1")), 1);
        assert_eq!(effective_slots(Some("-np=8")), 8);
        // Past the control ceiling the count clamps to it.
        assert_eq!(effective_slots(Some("--parallel 4096")), PARALLEL_SLOTS_MAX);
    }

    #[test]
    fn write_from_scratch_and_round_trip() {
        let written = set_parallel_slots(None, Some(1));
        assert_eq!(written.as_deref(), Some("--parallel 1"));
        assert_eq!(parallel_slots(written.as_deref()), Some(1));
        // Removing from nothing stays nothing.
        assert_eq!(set_parallel_slots(None, None), None);
        // Explicit auto writes the canonical token like any value.
        let written = set_parallel_slots(None, Some(LLAMA_CPP_PARALLEL_AUTO));
        assert_eq!(written.as_deref(), Some("--parallel -1"));
        assert_eq!(parallel_slots(written.as_deref()), Some(-1));
    }

    #[test]
    fn write_preserves_unrelated_flags_in_order() {
        // The planner-mtp canonical string: setting the slot
        // count it already carries keeps every token, with the
        // canonical --parallel placed at the end (the writer's
        // canonical placement, as in set_expert_offload).
        let before = "--parallel 1 --spec-type draft-mtp --spec-draft-n-max 2";
        assert_eq!(
            set_parallel_slots(Some(before), Some(1)).as_deref(),
            Some("--spec-type draft-mtp --spec-draft-n-max 2 --parallel 1")
        );
        // An already-canonical string round-trips exactly.
        let canonical = "--spec-type draft-mtp --spec-draft-n-max 2 --parallel 1";
        assert_eq!(
            set_parallel_slots(Some(canonical), Some(1)).as_deref(),
            Some(canonical)
        );
        // Offload tokens are not parallel's to touch.
        assert_eq!(
            set_parallel_slots(Some("-ot exps=CPU"), Some(2)).as_deref(),
            Some("-ot exps=CPU --parallel 2")
        );
    }

    #[test]
    fn write_strips_every_spelling() {
        // Pair, short, and = spellings all go; the canonical
        // token is appended once.
        assert_eq!(
            set_parallel_slots(Some("-np 4 --parallel=2 -c 8192"), Some(8)).as_deref(),
            Some("-c 8192 --parallel 8")
        );
        assert_eq!(
            set_parallel_slots(Some("--parallel 4 -np=3"), Some(1)).as_deref(),
            Some("--parallel 1")
        );
        // Removal strips the tokens and appends nothing.
        assert_eq!(
            set_parallel_slots(Some("-c 8192 --parallel 4"), None).as_deref(),
            Some("-c 8192")
        );
        assert_eq!(set_parallel_slots(Some("--parallel 4"), None), None);
        // The -1 value token is consumed with its flag (it parses
        // as a count, so it is not left behind as a stray token).
        assert_eq!(
            set_parallel_slots(Some("--parallel -1 --parallel 2"), None),
            None
        );
    }

    #[test]
    fn write_malformed_spellings_degrade_safely() {
        // A valueless trailing flag is removed and nothing else
        // is disturbed.
        assert_eq!(
            set_parallel_slots(Some("-c 8192 --parallel"), Some(1)).as_deref(),
            Some("-c 8192 --parallel 1")
        );
        // A malformed value token survives the rewrite (only the
        // flag is dropped, mirroring set_expert_offload).
        assert_eq!(
            set_parallel_slots(Some("--parallel abc"), None).as_deref(),
            Some("abc")
        );
        // A following flag is never eaten as a value.
        assert_eq!(
            set_parallel_slots(Some("--parallel -c 8192"), Some(2)).as_deref(),
            Some("-c 8192 --parallel 2")
        );
    }

    #[test]
    fn decode_of_write_is_the_written_value() {
        for raw in [
            None,
            Some(""),
            Some("-c 262144"),
            Some("--parallel 7 --spec-type draft-mtp"),
            Some("-np=2 -ot exps=CPU"),
        ] {
            for target in [None, Some(1), Some(16), Some(LLAMA_CPP_PARALLEL_AUTO)] {
                assert_eq!(
                    parallel_slots(set_parallel_slots(raw, target).as_deref()),
                    target,
                    "raw={raw:?} target={target:?}"
                );
            }
        }
    }
}
