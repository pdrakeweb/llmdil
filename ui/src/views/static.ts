/* Secondary views — all wired to the backend through api.ts.
   Each view follows the Setup wizard pattern: renderX() returns a shell
   with a loading placeholder; wireX(root) loads live data, paints it, and
   attaches handlers. Under Tauri the data is real; in a plain browser
   (live=false) views show a sample-mode banner and honest empty states
   instead of fabricated data. */

/* The secondary views are split one module per view (gpus, backends,
   models, logs, gateway, settings, wizard) plus their shared pieces
   (common, activationPlan, backendFlags, backendCard, settingsPanels).
   This file is the barrel: every name it exported before the split is
   re-exported here, so `import ... from "./views/static"` sites are
   unchanged. */

export {
  sameModelFile,
} from "./common";
export {
  renderGpus,
  wireGpus,
} from "./gpus";
export type {
  GpuModelRow,
} from "./gpus";
export {
  planApplyLine,
  activationApplying,
  applyActivationPlan,
  renderActivationPlan,
  wireActivationPlan,
} from "./activationPlan";
export {
  predictedCardTip,
  rawFlagWarnings,
} from "./backendFlags";
export type {
  FlagWarningLike,
  FlagsDraft,
  PlaceCardState,
} from "./backendFlags";
export {
  renderBackends,
} from "./backendCard";
export {
  wireBackends,
} from "./backends";
export {
  renderModels,
  wireModels,
} from "./models";
export {
  renderLogs,
  wireLogs,
} from "./logs";
export {
  renderGateway,
  wireGateway,
} from "./gateway";
export {
  renderSettings,
  wireSettings,
} from "./settings";
export {
  renderWizard,
  wireWizard,
} from "./wizard";
