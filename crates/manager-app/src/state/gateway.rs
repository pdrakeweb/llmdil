//! Gateway observation state: recent routing decisions, the LiteLLM
//! request-statistic store, and the per-backend direct `/slots`
//! store.
//!
//! Each store remains its own lock (none merged, none changed kind).
//! `list_backends` reads `direct_slots` and RELEASES it before it
//! locks `stats`; no path holds both. None of these is taken under
//! the supervisor lock (see the [`crate::state`] module docs).

use tokio::sync::Mutex;

use crate::gateway_stats;

/// The gateway's in-memory observation stores.
#[derive(Default)]
pub(crate) struct GatewayState {
    /// Recent gateway routing decisions (spec §14
    /// `gateway_recent_requests`). Every `gateway_test_routing`
    /// trace records here.
    pub(crate) routing_log: Mutex<manager_gateway::RoutingLog>,
    /// Gateway request-statistic snapshots scraped from LiteLLM's
    /// Prometheus endpoint by the background poller (see
    /// [`gateway_stats`]); `list_backends` turns them into the
    /// Dashboard's request/error figures (trailing hour) and live
    /// throughput rate (recent window).
    pub(crate) stats: Mutex<gateway_stats::GatewayStatsStore>,
    /// Per-backend llama-server `/slots` snapshots polled directly
    /// from each backend's own port by the background poller (see
    /// [`gateway_stats::DirectSlotsStore`]); `list_backends`
    /// prefers this live rate over the gateway rate, and falls
    /// back when a backend's `/slots` is unavailable.
    pub(crate) direct_slots: Mutex<gateway_stats::DirectSlotsStore>,
}
