//! MTP (multi-token prediction) speculative decoding in a
//! backend's raw llama-server flags: whether `--spec-type
//! draft-mtp` is configured, and the `--spec-draft-n-max` draft
//! length it drafts with.
//!
//! The decode/encode pair mirrors the expert-offload pair in
//! [`super::memory`] (`expert_offload` / `set_expert_offload`):
//! state is decoded from the saved raw flags — there is no
//! separate stored setting — and the writer rewrites only the
//! tokens the setting owns, leaving every other flag untouched
//! and in order. Malformed or hand-edited spellings degrade to a
//! safe decoded state, never a panic: an unparseable value reads
//! as unset, and a valueless flag the writer does not own is
//! preserved rather than deleted.

/// The draft length the app itself writes when MTP is enabled
/// without an explicit value — the control's default. Kept
/// distinct from llama.cpp's own default (below): an operator
/// enabling MTP through the app gets this length, while a
/// hand-written `--spec-type draft-mtp` with no length token
/// drafts at llama.cpp's.
pub const MTP_DEFAULT_N_MAX: u32 = 2;

/// llama.cpp's documented default for `--spec-draft-n-max` when
/// the flag is absent (llama-server(1)): a backend can carry
/// `--spec-type draft-mtp` with no draft-length token, and this
/// is the length it then drafts with.
pub const LLAMA_CPP_N_MAX_DEFAULT: u32 = 3;

/// The MTP control's draft-length range. The set op validates
/// against it; a hand-edited value outside it still decodes (the
/// control renders it as hand-set rather than inventing a
/// position).
pub const MTP_N_MAX_MIN: u32 = 1;
pub const MTP_N_MAX_MAX: u32 = 4;

/// MTP speculative decoding configured in a backend's raw flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MtpDraft {
    /// No `draft-mtp` in the effective `--spec-type` list.
    Off,
    /// `--spec-type draft-mtp` present. The payload is the
    /// explicit `--spec-draft-n-max` value, or `None` when no
    /// token sets it (llama.cpp's default then applies — see
    /// [`LLAMA_CPP_N_MAX_DEFAULT`]).
    On(Option<u32>),
}

impl MtpDraft {
    /// The draft length in effect when on: the explicit value,
    /// or llama.cpp's default when unset. `None` when off.
    pub fn effective_n_max(&self) -> Option<u32> {
        match self {
            MtpDraft::Off => None,
            MtpDraft::On(n) => Some(n.unwrap_or(LLAMA_CPP_N_MAX_DEFAULT)),
        }
    }
}

/// Split a `--spec-type` value into its type list (the flag takes
/// a comma-separated list; `none` is the no-speculation value
/// and only meaningful alone).
fn spec_types(value: &str) -> Vec<&str> {
    value
        .split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect()
}

/// The value token of a `--flag value` pair, for flags whose
/// value is a word (never begins with `-`): the next token when
/// it exists and is not itself a flag. Anything else reads as
/// valueless rather than eating the next flag's name.
fn pair_value<'a>(tokens: &[&'a str], i: usize) -> Option<&'a str> {
    tokens.get(i + 1).copied().filter(|v| !v.starts_with('-'))
}

/// The effective `--spec-type` list in raw flags: the last
/// occurrence wins, the sequential-parse rule llama.cpp follows
/// (the same rule [`super::memory::expert_offload`] documents
/// for a repeated `--n-cpu-moe`).
fn effective_spec_types<'a>(tokens: &[&'a str]) -> Vec<&'a str> {
    let mut types: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t == "--spec-type" {
            if let Some(v) = pair_value(tokens, i) {
                types = spec_types(v);
                i += 1;
            }
        } else if let Some(v) = t.strip_prefix("--spec-type=") {
            types = spec_types(v);
        }
        i += 1;
    }
    types
}

/// Detect MTP speculative decoding in raw llama-server flags.
///
/// `--spec-draft-n-max` resolves to the last value that parses
/// as a count, wherever it sits; a malformed value (non-numeric,
/// negative) is ignored rather than read as a length, so a
/// hand-edited flag degrades to the llama.cpp default instead
/// of a wrong number.
pub fn mtp_draft(raw_flags: Option<&str>) -> MtpDraft {
    let Some(raw) = raw_flags else {
        return MtpDraft::Off;
    };
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if !effective_spec_types(&tokens).contains(&"draft-mtp") {
        return MtpDraft::Off;
    }
    let mut n_max: Option<u32> = None;
    for (i, t) in tokens.iter().enumerate() {
        if *t == "--spec-draft-n-max" {
            if let Some(v) = tokens.get(i + 1) {
                if let Ok(n) = v.parse::<u32>() {
                    n_max = Some(n);
                }
            }
        } else if let Some(v) = t.strip_prefix("--spec-draft-n-max=") {
            if let Ok(n) = v.parse::<u32>() {
                n_max = Some(n);
            }
        }
    }
    MtpDraft::On(n_max)
}

/// Rewrite the MTP tokens in raw llama-server flags, leaving
/// every other flag untouched and in order. All `--spec-type`
/// occurrences are removed and a single canonical flag is
/// appended; `--spec-draft-n-max` is removed and re-appended for
/// the new setting. Returns `None` when nothing remains.
///
/// Sibling speculative types in the effective `--spec-type`
/// list (an ngram or eagle configuration sharing the flag) are
/// preserved across the rewrite — the writer owns `draft-mtp`
/// and the draft length, not the whole list. Turning MTP off
/// while sibling types remain keeps their `--spec-draft-n-max`
/// (it still serves them); turning it off with no siblings
/// removes the length token too. `On(None)` writes the app's
/// default length ([`MTP_DEFAULT_N_MAX`]) explicitly, so a
/// state the app wrote always carries its length token.
pub fn set_mtp_draft(raw_flags: Option<&str>, setting: MtpDraft) -> Option<String> {
    let tokens: Vec<&str> = raw_flags.unwrap_or("").split_whitespace().collect();
    // Sibling types from the effective list, in order. "none" is
    // dropped when real types remain: it is only meaningful
    // alone, as the no-speculation value.
    let others: Vec<&str> = effective_spec_types(&tokens)
        .into_iter()
        .filter(|p| *p != "draft-mtp" && *p != "none")
        .collect();
    let keep_n_max = setting == MtpDraft::Off && !others.is_empty();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t == "--spec-type" {
            match pair_value(&tokens, i) {
                Some(_) => {
                    i += 2;
                    continue;
                }
                // A valueless --spec-type is malformed but not
                // ours to delete.
                None => {
                    out.push(t.to_string());
                    i += 1;
                    continue;
                }
            }
        }
        if t.starts_with("--spec-type=") {
            i += 1;
            continue;
        }
        if !keep_n_max {
            if t == "--spec-draft-n-max" {
                // Skip the value token too when it parses as a
                // count; a malformed value is left for the main
                // walk (mirrors set_expert_offload).
                let has_value = tokens
                    .get(i + 1)
                    .map(|v| v.parse::<u32>().is_ok())
                    .unwrap_or(false);
                i += if has_value { 2 } else { 1 };
                continue;
            }
            if t.starts_with("--spec-draft-n-max=") {
                i += 1;
                continue;
            }
        }
        out.push(t.to_string());
        i += 1;
    }
    match setting {
        MtpDraft::On(n) => {
            let mut list: Vec<&str> = others.clone();
            list.push("draft-mtp");
            out.push("--spec-type".to_string());
            out.push(list.join(","));
            out.push("--spec-draft-n-max".to_string());
            out.push(n.unwrap_or(MTP_DEFAULT_N_MAX).to_string());
        }
        MtpDraft::Off => {
            if !others.is_empty() {
                out.push("--spec-type".to_string());
                out.push(others.join(","));
            }
        }
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
    fn decode_off_states() {
        assert_eq!(mtp_draft(None), MtpDraft::Off);
        assert_eq!(mtp_draft(Some("")), MtpDraft::Off);
        assert_eq!(mtp_draft(Some("--parallel 1 -c 262144")), MtpDraft::Off);
        assert_eq!(mtp_draft(Some("--spec-type none")), MtpDraft::Off);
        assert_eq!(mtp_draft(Some("--spec-type ngram-cache")), MtpDraft::Off);
        // A draft length without the type is not MTP.
        assert_eq!(mtp_draft(Some("--spec-draft-n-max 2")), MtpDraft::Off);
    }

    #[test]
    fn decode_on_without_n_max_uses_llama_cpp_default() {
        let d = mtp_draft(Some("--parallel 1 --spec-type draft-mtp"));
        assert_eq!(d, MtpDraft::On(None));
        assert_eq!(d.effective_n_max(), Some(LLAMA_CPP_N_MAX_DEFAULT));
    }

    #[test]
    fn decode_on_with_n_max_both_spellings() {
        assert_eq!(
            mtp_draft(Some("--spec-type draft-mtp --spec-draft-n-max 2")),
            MtpDraft::On(Some(2))
        );
        assert_eq!(
            mtp_draft(Some("--spec-type=draft-mtp --spec-draft-n-max=4")),
            MtpDraft::On(Some(4))
        );
        // Comma list sharing the flag.
        assert_eq!(
            mtp_draft(Some("--spec-type ngram-cache,draft-mtp --spec-draft-n-max 1")),
            MtpDraft::On(Some(1))
        );
    }

    #[test]
    fn decode_last_spec_type_wins() {
        assert_eq!(
            mtp_draft(Some("--spec-type draft-mtp --spec-type ngram-cache")),
            MtpDraft::Off
        );
        assert_eq!(
            mtp_draft(Some("--spec-type ngram-cache --spec-type draft-mtp")),
            MtpDraft::On(None)
        );
    }

    #[test]
    fn decode_malformed_values_degrade() {
        // Non-numeric and negative lengths read as unset.
        assert_eq!(
            mtp_draft(Some("--spec-type draft-mtp --spec-draft-n-max abc")),
            MtpDraft::On(None)
        );
        assert_eq!(
            mtp_draft(Some("--spec-type draft-mtp --spec-draft-n-max -1")),
            MtpDraft::On(None)
        );
        // The last PARSEABLE length wins over a later malformed one.
        assert_eq!(
            mtp_draft(Some(
                "--spec-type draft-mtp --spec-draft-n-max 2 --spec-draft-n-max x"
            )),
            MtpDraft::On(Some(2))
        );
        // A repeated length resolves to the last one.
        assert_eq!(
            mtp_draft(Some(
                "--spec-type draft-mtp --spec-draft-n-max 2 --spec-draft-n-max 3"
            )),
            MtpDraft::On(Some(3))
        );
        // A trailing valueless --spec-type never panics and is off.
        assert_eq!(mtp_draft(Some("--parallel 1 --spec-type")), MtpDraft::Off);
        // A hand-set length outside the control's range decodes
        // as-is; the control renders it as hand-set.
        assert_eq!(
            mtp_draft(Some("--spec-type draft-mtp --spec-draft-n-max 7")),
            MtpDraft::On(Some(7))
        );
        assert_eq!(
            mtp_draft(Some("--spec-type draft-mtp --spec-draft-n-max 0")),
            MtpDraft::On(Some(0))
        );
    }

    #[test]
    fn write_enable_from_scratch_and_round_trip() {
        let written = set_mtp_draft(None, MtpDraft::On(Some(2)));
        assert_eq!(
            written.as_deref(),
            Some("--spec-type draft-mtp --spec-draft-n-max 2")
        );
        assert_eq!(mtp_draft(written.as_deref()), MtpDraft::On(Some(2)));
        // On(None) writes the app default explicitly.
        let written = set_mtp_draft(None, MtpDraft::On(None));
        assert_eq!(
            written.as_deref(),
            Some("--spec-type draft-mtp --spec-draft-n-max 2")
        );
        assert_eq!(mtp_draft(written.as_deref()), MtpDraft::On(Some(2)));
    }

    #[test]
    fn write_preserves_unrelated_flags_in_order() {
        let before = "--parallel 1 -c 262144 --flash-attn on";
        let after = set_mtp_draft(Some(before), MtpDraft::On(Some(3)));
        assert_eq!(
            after.as_deref(),
            Some("--parallel 1 -c 262144 --flash-attn on --spec-type draft-mtp --spec-draft-n-max 3")
        );
        // The app's own planner-mtp spelling round-trips exactly.
        let canonical = "--parallel 1 --spec-type draft-mtp --spec-draft-n-max 2";
        assert_eq!(
            set_mtp_draft(Some(canonical), MtpDraft::On(Some(2))).as_deref(),
            Some(canonical)
        );
        // Changing only the length keeps every other token.
        assert_eq!(
            set_mtp_draft(Some(canonical), MtpDraft::On(Some(4))).as_deref(),
            Some("--parallel 1 --spec-type draft-mtp --spec-draft-n-max 4")
        );
    }

    #[test]
    fn write_off_removes_both_tokens() {
        let before = "--parallel 1 --spec-type draft-mtp --spec-draft-n-max 2";
        assert_eq!(
            set_mtp_draft(Some(before), MtpDraft::Off).as_deref(),
            Some("--parallel 1")
        );
        // Off from an only-MTP flag string leaves nothing.
        assert_eq!(
            set_mtp_draft(Some("--spec-type draft-mtp --spec-draft-n-max 2"), MtpDraft::Off),
            None
        );
        // Off when already off is a no-op rewrite.
        assert_eq!(
            set_mtp_draft(Some("--parallel 1"), MtpDraft::Off).as_deref(),
            Some("--parallel 1")
        );
    }

    #[test]
    fn write_preserves_sibling_spec_types() {
        // Enabling merges draft-mtp into the effective list.
        assert_eq!(
            set_mtp_draft(Some("--spec-type ngram-cache"), MtpDraft::On(Some(3))).as_deref(),
            Some("--spec-type ngram-cache,draft-mtp --spec-draft-n-max 3")
        );
        // Disabling removes only draft-mtp; the sibling keeps its
        // draft length. The rebuilt --spec-type lands at the end
        // (the writer's canonical placement, as in
        // set_expert_offload); the length token stays in place.
        assert_eq!(
            set_mtp_draft(
                Some("--spec-type ngram-cache,draft-mtp --spec-draft-n-max 5"),
                MtpDraft::Off
            )
            .as_deref(),
            Some("--spec-draft-n-max 5 --spec-type ngram-cache")
        );
        // "none" alone is replaced, not merged.
        assert_eq!(
            set_mtp_draft(Some("--spec-type none"), MtpDraft::On(Some(2))).as_deref(),
            Some("--spec-type draft-mtp --spec-draft-n-max 2")
        );
    }

    #[test]
    fn write_malformed_spellings_degrade_safely() {
        // A valueless trailing --spec-type is preserved verbatim.
        assert_eq!(
            set_mtp_draft(Some("--parallel 1 --spec-type"), MtpDraft::Off).as_deref(),
            Some("--parallel 1 --spec-type")
        );
        // A malformed length value token survives an off rewrite
        // (only the flag is dropped, mirroring set_expert_offload).
        assert_eq!(
            set_mtp_draft(
                Some("--spec-type draft-mtp --spec-draft-n-max abc"),
                MtpDraft::Off
            )
            .as_deref(),
            Some("abc")
        );
        // `=` spellings are owned and removed.
        assert_eq!(
            set_mtp_draft(
                Some("--spec-type=draft-mtp --spec-draft-n-max=2"),
                MtpDraft::Off
            ),
            None
        );
    }
}
