/* Whether the setup wizard opens by itself on launch (only asked while
   setup is not marked complete). */

import type { LifecycleState } from "./lifecycle-state";

/** The wizard steps the app cannot run without. */
const REQUIRED_STEPS = ["detect-hardware", "install-llamacpp", "start-gateway"];

/** True when a required part is missing: no configured backend, no
 *  gateway installed, or a required step that failed. A null input is
 *  unknown and never opens the wizard. */
export function setupNeedsWizard(i: {
  backends: readonly { configured: boolean }[] | null;
  gateway: LifecycleState | null;
  steps: readonly { id: string; state: string }[];
}): boolean {
  if (i.backends && !i.backends.some((b) => b.configured)) return true;
  if (i.gateway === "not_installed") return true;
  return i.steps.some((s) => REQUIRED_STEPS.includes(s.id) && s.state === "failed");
}
