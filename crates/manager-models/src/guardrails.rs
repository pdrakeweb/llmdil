//! Per-model × per-architecture expert-offload guardrails.
//!
//! Some offload positions are known-bad on a specific GPU
//! architecture even though the same position is the model's
//! design point elsewhere. The incident behind this module:
//! Qwen3-Coder-Next-80B at `--n-cpu-moe 46` on a Tesla P100
//! (compute capability (6, 0)) died with a CUDA launch error
//! (cudaErrorInvalidArgument); full expert offload fixed it.
//! Determination: guardrails are pinned per model × per
//! architecture in the curated catalog
//! ([`ModelSpec::offload_restrictions`](super::ModelSpec)), and a
//! position is evaluated against the architectures a backend is
//! actually placed on before it is offered or launched.
//!
//! The evaluation is deliberately conservative about what it
//! claims: a position a matching restriction does not allow is
//! [`OffloadStatus::Blocked`], a position on an architecture the
//! catalog says nothing about is [`OffloadStatus::Verified`] only
//! when the architecture is known, and a partial position whose
//! architecture is unknown is [`OffloadStatus::Untested`] — the
//! restrictions cannot be consulted without an architecture to
//! match them against, so nothing is asserted either way.

use serde::{Deserialize, Serialize};

use super::ModelSpec;

/// Which offload positions a restriction allows on its
/// architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllowedOffload {
    /// Only the endpoints of the offload range: no offload
    /// (0 layers) or full expert offload (all MoE layers).
    /// Partial positions are blocked.
    EndpointsOnly,
}

impl AllowedOffload {
    /// Whether this allowance is satisfied by offloading
    /// `layers` of `moe_layers` MoE layers.
    pub fn allows(&self, layers: u32, moe_layers: u32) -> bool {
        match self {
            AllowedOffload::EndpointsOnly => layers == 0 || layers == moe_layers,
        }
    }
}

/// One offload restriction: on GPUs with this compute capability,
/// only the positions in `allowed` may be used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OffloadRestriction {
    /// CUDA compute capability `(major, minor)` the restriction
    /// applies to, e.g. `(6, 0)` for Pascal (Tesla P100).
    pub compute_capability: (u32, u32),
    pub allowed: AllowedOffload,
}

/// Verification status of one offload position for a model on a
/// concrete placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OffloadStatus {
    /// The position is allowed: either no restriction governs it
    /// or every governing restriction is satisfied. Also the
    /// status of any position evaluated CPU-only (no GPU expert
    /// compute happens at all) and of endpoint positions whose
    /// architecture is unknown (endpoints are allowed by every
    /// restriction kind).
    Verified,
    /// The position cannot be verified: the placed architecture
    /// is unknown and the position is partial, so a restriction
    /// might govern it but cannot be matched.
    Untested,
    /// A restriction matching a placed GPU forbids the position.
    Blocked,
}

/// The restriction in `spec` governing compute capability `cc`,
/// if the catalog pins one for that architecture.
pub fn restriction_for(spec: &ModelSpec, cc: (u32, u32)) -> Option<&OffloadRestriction> {
    spec.offload_restrictions
        .iter()
        .find(|r| r.compute_capability == cc)
}

/// Evaluate the offload position `layers` (of `moe_layers`) for a
/// model carrying `restrictions`, placed on GPUs whose compute
/// capabilities are `placed_ccs`.
///
/// - `cpu_only` short-circuits to [`OffloadStatus::Verified`]: no
///   GPU expert compute happens at all, so no architecture
///   restriction can bite.
/// - When at least one placed compute capability matches a
///   restriction, the position must satisfy *every* matching
///   restriction: satisfied is [`OffloadStatus::Verified`], any
///   violation is [`OffloadStatus::Blocked`]. One restricted GPU
///   in a split placement governs the whole position.
/// - Known placed capabilities with no matching restriction are
///   [`OffloadStatus::Verified`].
/// - An empty `placed_ccs` means the architecture is unknown:
///   restrictions cannot be matched, so a partial position is
///   [`OffloadStatus::Untested`] and the endpoints — allowed by
///   every restriction kind — are [`OffloadStatus::Verified`].
pub fn offload_status(
    restrictions: &[OffloadRestriction],
    layers: u32,
    moe_layers: u32,
    placed_ccs: &[(u32, u32)],
    cpu_only: bool,
) -> OffloadStatus {
    if cpu_only {
        return OffloadStatus::Verified;
    }
    if placed_ccs.is_empty() {
        return if layers == 0 || layers == moe_layers {
            OffloadStatus::Verified
        } else {
            OffloadStatus::Untested
        };
    }
    for restriction in restrictions {
        if placed_ccs.contains(&restriction.compute_capability)
            && !restriction.allowed.allows(layers, moe_layers)
        {
            return OffloadStatus::Blocked;
        }
    }
    OffloadStatus::Verified
}

/// The nearest allowed position to `layers` under the restriction
/// governing compute capability `cc`, for snapping a blocked (or
/// about-to-be-blocked) slider position to a legal one. When no
/// restriction matches `cc`, `layers` is returned unchanged.
///
/// For [`AllowedOffload::EndpointsOnly`] the nearer endpoint
/// wins; a tie resolves to `moe_layers` — full offload is the
/// restricted model's design point, so the midpoint snaps there
/// rather than to no offload.
pub fn nearest_allowed(
    restrictions: &[OffloadRestriction],
    cc: (u32, u32),
    layers: u32,
    moe_layers: u32,
) -> u32 {
    let Some(restriction) = restrictions.iter().find(|r| r.compute_capability == cc) else {
        return layers;
    };
    match restriction.allowed {
        AllowedOffload::EndpointsOnly => {
            if restriction.allowed.allows(layers, moe_layers) {
                return layers;
            }
            // Distance to each endpoint; the full-offload endpoint
            // wins ties (`<=`), per the design-point rule above.
            let to_full = moe_layers.saturating_sub(layers);
            if to_full <= layers {
                moe_layers
            } else {
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curated_models;

    fn spec(id: &str) -> ModelSpec {
        curated_models()
            .into_iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("catalog model {id}"))
    }

    fn coder() -> ModelSpec {
        spec("qwen3-coder-30b")
    }

    #[test]
    fn coder_next_pascal_allows_endpoints_only() {
        let coder = coder();
        let restrictions = &coder.offload_restrictions;
        for layers in [0, 48] {
            assert_eq!(
                offload_status(restrictions, layers, 48, &[(6, 0)], false),
                OffloadStatus::Verified,
                "layers {layers}"
            );
        }
        for layers in [1, 24, 46, 47] {
            assert_eq!(
                offload_status(restrictions, layers, 48, &[(6, 0)], false),
                OffloadStatus::Blocked,
                "layers {layers}"
            );
        }
    }

    #[test]
    fn coder_next_unrestricted_architecture_allows_every_position() {
        let coder = coder();
        for layers in [0, 1, 24, 46, 47, 48] {
            assert_eq!(
                offload_status(&coder.offload_restrictions, layers, 48, &[(8, 6)], false),
                OffloadStatus::Verified,
                "layers {layers}"
            );
        }
    }

    #[test]
    fn nemotron_partial_design_point_is_verified_everywhere() {
        // Nemotron carries no restriction: its 16-of-24 partial
        // design point must stay allowed on Pascal and elsewhere.
        let nemotron = spec("nemotron-3.5-lightning-30b");
        assert!(nemotron.offload_restrictions.is_empty());
        for cc in [(6, 0), (8, 6)] {
            assert_eq!(
                offload_status(&nemotron.offload_restrictions, 16, 24, &[cc], false),
                OffloadStatus::Verified,
                "cc {cc:?}"
            );
        }
    }

    #[test]
    fn unknown_architecture_marks_partial_positions_untested() {
        // Restrictions exist but cannot be matched without a
        // placed architecture: partial positions are Untested,
        // endpoints (allowed by every restriction kind) Verified.
        let coder = coder();
        assert_eq!(
            offload_status(&coder.offload_restrictions, 46, 48, &[], false),
            OffloadStatus::Untested
        );
        for layers in [0, 48] {
            assert_eq!(
                offload_status(&coder.offload_restrictions, layers, 48, &[], false),
                OffloadStatus::Verified,
                "layers {layers}"
            );
        }
    }

    #[test]
    fn split_placement_any_restricted_gpu_governs() {
        let coder = coder();
        assert_eq!(
            offload_status(
                &coder.offload_restrictions,
                46,
                48,
                &[(8, 6), (6, 0)],
                false
            ),
            OffloadStatus::Blocked
        );
    }

    #[test]
    fn cpu_only_is_verified_at_any_position() {
        let coder = coder();
        for layers in [0, 1, 24, 46, 48] {
            assert_eq!(
                offload_status(&coder.offload_restrictions, layers, 48, &[(6, 0)], true),
                OffloadStatus::Verified,
                "layers {layers}"
            );
        }
    }

    #[test]
    fn nearest_allowed_snaps_to_the_nearer_endpoint() {
        let coder = coder();
        let restrictions = &coder.offload_restrictions;
        assert_eq!(nearest_allowed(restrictions, (6, 0), 46, 48), 48);
        assert_eq!(nearest_allowed(restrictions, (6, 0), 10, 48), 0);
        // The endpoints themselves are already allowed.
        assert_eq!(nearest_allowed(restrictions, (6, 0), 0, 48), 0);
        assert_eq!(nearest_allowed(restrictions, (6, 0), 48, 48), 48);
    }

    #[test]
    fn nearest_allowed_tie_resolves_to_full_offload() {
        let coder = coder();
        assert_eq!(
            nearest_allowed(&coder.offload_restrictions, (6, 0), 24, 48),
            48
        );
    }

    #[test]
    fn nearest_allowed_without_a_matching_restriction_is_identity() {
        let coder = coder();
        // A known architecture no restriction governs.
        assert_eq!(
            nearest_allowed(&coder.offload_restrictions, (8, 6), 23, 48),
            23
        );
        // A model with no restrictions at all.
        let nemotron = spec("nemotron-3.5-lightning-30b");
        assert_eq!(
            nearest_allowed(&nemotron.offload_restrictions, (6, 0), 16, 24),
            16
        );
    }

    #[test]
    fn restriction_for_finds_the_matching_architecture() {
        let coder = coder();
        let restriction = restriction_for(&coder, (6, 0)).expect("Pascal restriction");
        assert_eq!(restriction.allowed, AllowedOffload::EndpointsOnly);
        assert_eq!(restriction_for(&coder, (8, 6)), None);
        let nemotron = spec("nemotron-3.5-lightning-30b");
        assert_eq!(restriction_for(&nemotron, (6, 0)), None);
    }

    #[test]
    fn guardrail_types_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&AllowedOffload::EndpointsOnly).unwrap(),
            "\"endpoints_only\""
        );
        assert_eq!(
            serde_json::to_string(&OffloadStatus::Verified).unwrap(),
            "\"verified\""
        );
        assert_eq!(
            serde_json::to_string(&OffloadStatus::Untested).unwrap(),
            "\"untested\""
        );
        assert_eq!(
            serde_json::to_string(&OffloadStatus::Blocked).unwrap(),
            "\"blocked\""
        );
        let restriction = OffloadRestriction {
            compute_capability: (6, 0),
            allowed: AllowedOffload::EndpointsOnly,
        };
        let round_tripped: OffloadRestriction =
            serde_json::from_str(&serde_json::to_string(&restriction).unwrap()).unwrap();
        assert_eq!(round_tripped, restriction);
    }
}
