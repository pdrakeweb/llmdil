import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";

/* ---------------- Debug console (docs/debug-console.md) ----------------
 * Thin wrappers over the console_* registry operations. Every one is
 * also an admin API route; the desktop never passes `caller` (it is
 * the local operator). Results are passed through as JSON: the view
 * renders the server's `text` and structured fields directly. */

export interface ConsoleExecResult {
  ok: boolean;
  line: string;
  command?: string;
  text: string;
  error?: string;
  data?: Record<string, unknown>;
}

export interface ConsoleOutputLine { seq: number; ts_ms: number; step: number | null; stream: string; line: string }

export interface ConsoleOutput {
  run_id: string;
  status: string;
  done: boolean;
  lines: ConsoleOutputLine[];
  next: number;
  more?: boolean;
}

export type Json = Record<string, unknown>;

async function consoleInvoke<T>(cmd: string, call: () => Promise<T>, fallback: T): Promise<CallResult<T>> {
  if (!isTauri()) return { data: fallback, live: false, error: `${cmd}: not connected` };
  try {
    return { data: await call(), live: true };
  } catch (e) {
    return { data: fallback, live: true, error: errOf(e) };
  }
}

export function consoleExec(line: string, sessionId: string): Promise<CallResult<ConsoleExecResult>> {
  return consoleInvoke("console_exec", () => invoke<ConsoleExecResult>("console_exec", { line, sessionId }),
    { ok: false, line, text: "" });
}

export function consoleImport(text: string, name: string | null, fold: boolean): Promise<CallResult<Json>> {
  return consoleInvoke("console_import", () => invoke<Json>("console_import", { text, name, fold, save: false }), {});
}

export function consolePlan(recipeDoc: Json, params: Record<string, string>): Promise<CallResult<Json>> {
  return consoleInvoke("console_plan", () => invoke<Json>("console_plan", { recipeDoc, params }), {});
}

export function consoleRunStart(recipeDoc: Json, params: Record<string, string>, variant: number | null,
  confirmStop: string[]): Promise<CallResult<Json>> {
  return consoleInvoke("console_run_start",
    () => invoke<Json>("console_run_start", { recipeDoc, params, variant, confirmStop }), {});
}

export function consoleRunCancel(runId: string): Promise<CallResult<Json>> {
  return consoleInvoke("console_run_cancel", () => invoke<Json>("console_run_cancel", { runId }), {});
}

export function consoleRunOutput(runId: string, since: number): Promise<CallResult<ConsoleOutput>> {
  return consoleInvoke("console_run_output",
    () => invoke<ConsoleOutput>("console_run_output", { runId, since, limit: 2000 }),
    { run_id: runId, status: "unknown", done: false, lines: [], next: since });
}

export function consoleRunResult(runId: string, full = false): Promise<CallResult<Json>> {
  return consoleInvoke("console_run_result", () => invoke<Json>("console_run_result", { runId, full }), {});
}

export function consoleRunsList(): Promise<CallResult<Json>> {
  return consoleInvoke("console_runs_list", () => invoke<Json>("console_runs_list", { limit: 100 }), { runs: [] });
}

export function consoleRunExport(runId: string): Promise<CallResult<Json>> {
  return consoleInvoke("console_run_export", () => invoke<Json>("console_run_export", { runId }), {});
}

export function consoleRecipesList(): Promise<CallResult<Json>> {
  return consoleInvoke("console_recipes_list", () => invoke<Json>("console_recipes_list"), { recipes: [] });
}

export function consoleRecipeGet(name: string): Promise<CallResult<Json>> {
  return consoleInvoke("console_recipe_get", () => invoke<Json>("console_recipe_get", { name }), {});
}

export function consoleRecipeSave(recipe: Json, overwrite: boolean): Promise<CallResult<Json>> {
  return consoleInvoke("console_recipe_create",
    () => invoke<Json>("console_recipe_create", { recipe, overwrite }), {});
}

export function consoleRecipeDelete(name: string): Promise<CallResult<Json>> {
  return consoleInvoke("console_recipe_delete", () => invoke<Json>("console_recipe_delete", { name }), {});
}

export function consoleSettingsGet(): Promise<CallResult<Json>> {
  return consoleInvoke("console_settings_get", () => invoke<Json>("console_settings_get"), {});
}

export function consoleSettingsSet(allowRemoteExec: boolean, apiRawShell: boolean, execAllowlist: string[]): Promise<CallResult<Json>> {
  return consoleInvoke("console_settings_set",
    () => invoke<Json>("console_settings_set", { allowRemoteExec, apiRawShell, execAllowlist }), {});
}

export function consoleShellOpen(sessionId: string): Promise<CallResult<Json>> {
  return consoleInvoke("console_shell_open", () => invoke<Json>("console_shell_open", { sessionId }), {});
}

export function consoleShellSend(sessionId: string, line: string): Promise<CallResult<Json>> {
  return consoleInvoke("console_shell_send", () => invoke<Json>("console_shell_send", { sessionId, line }), {});
}

export function consoleShellRead(sessionId: string, since: number): Promise<CallResult<Json>> {
  return consoleInvoke("console_shell_read", () => invoke<Json>("console_shell_read", { sessionId, since }), { lines: [] });
}

export function consoleShellClose(sessionId: string): Promise<CallResult<Json>> {
  return consoleInvoke("console_shell_close", () => invoke<Json>("console_shell_close", { sessionId }), {});
}
