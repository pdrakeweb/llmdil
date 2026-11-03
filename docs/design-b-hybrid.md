# Design B Hybrid — decision record

Pete's selection (Oct 8, 2026): **Design B (Appliance Console) as the base**, with Design C's
cluster diagrams and Design A's console density integrated as alternate views.

## Core pattern: view switcher

Data-dense windows carry a segmented **View** selector in the window toolbar, e.g.
`View: Appliance | Console | Topology`. The selector switches the presentation of the
*same underlying data* — not a different page. The choice persists per window across
sessions. Appliance is the default everywhere.

- **Appliance** — Design B's guided, chart-forward cards (the default).
- **Console** — Design A's dense, keyboard-friendly tables and streams for the same data.
- **Topology / Graph** — Design C's node-map rendering of the same data.

## Per-window view matrix

| Window | Appliance (default) | Console | Topology / Graph |
|---|---|---|---|
| Setup wizard | Guided wizard with auto-run checklist | — | — |
| Dashboard | Health gauge cards, 24h chart | Dense service/GPU table | Cluster service map (mini) |
| Service map | — | — | New first-class window (C-02 re-skinned in B language); nav item + deep-link from dashboard Topology tab |
| GPUs | Radial gauges, sparkline strips | A-style GPU detail tables, process attribution | — |
| Backend detail | Stat tiles, TTFT chart, health ticks | A-style dense backend table + flags | Per-backend request-flow graph |
| Models | Cards + fit bars (already dense) | — | — |
| Logs | Icon rows, level-distribution bar | A-style dense text stream | — |
| Gateway | Rule cards, latency sparklines | — | Routing-flow overlay (rule → group → backend) |
| Settings / MXC | Policy tiles, shield chips | — | — |

## What was built to demonstrate it

`graphics/design-b-hybrid/` — 15 mockups at 1600×1000, all in B's visual language.
**All 8 original Design B windows are preserved** (01 setup wizard, 02 dashboard,
06 GPUs, 08 backend detail, 10 models, 11 logs, 13 gateway, 15 settings/MXC);
console and topology views are additions, not replacements:

1. `01-setup-wizard.png` — B setup wizard with auto-run checklist, completion bar, re-run audit (Appliance)
2. `02-dashboard-appliance.png` — B dashboard, View selector visible, Appliance active
3. `03-dashboard-console.png` — same data, Console active (A density, B styling)
4. `04-dashboard-topology.png` — same data, Topology active (C node map, B styling)
5. `05-service-map.png` — full service map window, C-02 re-skinned in B's language
6. `06-gpus-appliance.png` — B GPUs window (radial gauges, sparkline strips)
7. `07-gpus-console.png` — GPUs window, Console view
8. `08-backend-appliance.png` — B backend detail (stat tiles, TTFT chart, health ticks)
9. `09-backend-graph.png` — backend detail, Graph view (request-flow topology)
10. `10-models.png` — B models window (fit bars, per-GPU fit, checksums)
11. `11-logs-appliance.png` — B logs window (icon rows, level-distribution bar)
12. `12-logs-console.png` — logs window, Console view (dense stream)
13. `13-gateway-appliance.png` — B gateway window (rule cards, model groups, latency sparklines)
14. `14-gateway-routing-map.png` — gateway window with routing-flow map overlay
15. `15-settings-mxc.png` — B settings/MXC window (policy tiles, shield chips)

Gallery: `graphics/design-b-hybrid/index.html`.

## Implementation notes (for the Tauri build)

- One data layer, three presenters per window: the switcher is a view-mode state, not a
  route. Console and Topology views subscribe to the same telemetry store as Appliance.
- Persist `windowId → viewMode` in the app config; Appliance is the fallback default.
- The service map is both a window (nav: Service Map) and the Dashboard's Topology mode
  (mini variant); the full window deep-links from the mini (node click → full map).
- Console views reuse the same icon set and status vocabulary as Appliance; density
  comes from table layout and smaller type, not a different design language.
- Gateway routing map reuses the service-map node renderer with rule→group→backend edges.

## Test plan additions

- Switcher present and functional on dashboard, GPUs, backend detail, logs; absent where
  the matrix says so.
- View choice persists across restart per window; unknown/corrupt value falls back to
  Appliance.
- Console and Topology views show identical values to Appliance for the same timestamp
  (same store, no divergent queries).
- Service map reachable from nav and from dashboard Topology mode; node click deep-links
  to the full map with the node highlighted.
- Keyboard: arrow keys or 1/2/3 switch views when the window is focused (A's
  keyboard-driven ethos, B's discoverability).
