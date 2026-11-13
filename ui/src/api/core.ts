/* Tauri command boundary: the shared invoke plumbing (see ../api.ts).
   Under Tauri, invoke() calls window.__TAURI__.core.invoke; in a plain
   browser isTauri() is false and callers fall back to sample data. */

import type { OpName } from "../generated/ops";

declare global {
  interface Window {
    __TAURI__?: {
      core: { invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> };
      /** Tauri's global event API (withGlobalTauri). Optional: absent
       *  outside Tauri and on shells built without it. */
      event?: { listen(name: string, cb: (e: { payload: unknown }) => void): Promise<() => void> };
    };
  }
}

export function isTauri(): boolean {
  return typeof window.__TAURI__?.core.invoke === "function";
}

/** The desktop shell's own commands (admin API lifecycle and user
 *  administration) — not registry operations; see SHELL_LOCAL in
 *  crates/manager-app/src/registry.rs. */
type ShellCommand =
  | "admin_api_get_status"
  | "admin_api_set_enabled"
  | "admin_api_list_users"
  | "admin_api_create_user"
  | "admin_api_update_user"
  | "admin_api_delete_user"
  | "admin_api_reset_password"
  | "admin_api_get_pairing"
  | "admin_api_create_enrollment"
  | "admin_api_regenerate_cert"
  | "admin_api_apply_firewall_rule"
  | "admin_api_get_audit";

/** Every command name the UI may send: a registered operation
 *  (generated from the registry) or a shell-local command. */
export type CommandName = OpName | ShellCommand;

export async function invoke<T>(cmd: CommandName, args?: Record<string, unknown>): Promise<T> {
  const fn = window.__TAURI__?.core.invoke;
  if (!fn) throw new Error("not under Tauri");
  return fn<T>(cmd, args);
}

export interface CallResult<T> {
  data: T;
  live: boolean;
  /** Set when the backend call failed; data is degraded (empty/placeholder). */
  error?: string;
}

export function errOf(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
