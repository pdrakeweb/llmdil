/* Shared two-position enable/disable switch (Pete's directive,
   2026-10-18; design rule 16). The control shows STATE, never the
   action a click will take: enabled = green track, knob right,
   check glyph; disabled = gray track, knob left, slash (ban)
   glyph. This replaces the old action-icon buttons, which showed
   a slash on enabled entities and a check on disabled ones (the
   icon named the action), and the On/Off and Enable/Disable word
   buttons elsewhere.

   Rendered as a native <button role="switch">: keyboard-operable
   (Enter/Space fire click, as for any button) and state-exposed
   via aria-checked. The caller passes the exact id / data-* hooks
   the replaced button carried in `attrs`, so every existing
   delegated click handler resolves the switch through its usual
   closest() lookup and fires the identical command — handlers,
   confirmations, and guards are untouched by the swap. */

import { icon } from "./icons";
import { esc } from "./html";

export interface EnableSwitchOpts {
  /** Current state: true = enabled. */
  on: boolean;
  /** Entity the switch controls, e.g. "Backend coder". Combined
   *  with the state into the tooltip and accessible name
   *  ("Backend coder: enabled"). */
  name: string;
  /** Raw attribute string the replaced button carried (id,
   *  data-* hooks), inserted verbatim — callers escape values
   *  interpolated into it, as everywhere else in the views. */
  attrs?: string;
  /** Tooltip override; defaults to "<name>: enabled|disabled".
   *  Use when the plain state text needs a qualifier (e.g. the
   *  backend that must be stopped before it can be disabled, or
   *  an MCP server's exposure detail). */
  title?: string;
  /** Native disabled: in-flight action, a state the command layer
   *  refuses to toggle from, or gated by a master switch. */
  disabled?: boolean;
}

export function enableSwitch(o: EnableSwitchOpts): string {
  const state = o.on ? "enabled" : "disabled";
  const label = `${o.name}: ${state}`;
  return `<button type="button" role="switch" aria-checked="${o.on}" class="switch"${o.attrs ? ` ${o.attrs}` : ""} title="${esc(o.title ?? label)}" aria-label="${esc(label)}"${o.disabled ? " disabled" : ""}><span class="switch-track"><span class="switch-knob">${icon(o.on ? "check" : "ban")}</span></span></button>`;
}
