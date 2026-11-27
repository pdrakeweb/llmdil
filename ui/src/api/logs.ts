import { errOf, invoke, isTauri } from "./core";
import type { CallResult } from "./core";

/* ---------------- Logs ---------------- */

export interface LogRow {
  ts: string;
  stream: string;
  line: string;
  /** Process id this line was captured from (its scope). */
  source: string;
  /** True when the line comes from a previous run's retained ring. */
  archived: boolean;
}

/** One entry of the payload's scope list: a queryable log scope.
 *  `previous` marks a scope that also has a previous run's retained
 *  ring, labeled "<id> (incl. previous run)" in the picker. */
export interface LogScopeInfo {
  id: string;
  previous: boolean;
}

export interface LogPage {
  lines: LogRow[];
  /** Lines matching scope + filters before paging was applied. */
  total_matching: number;
  /** Alias of total_matching, returned by current backends. */
  total: number;
  /** True when paging cut the result on either side of the page. */
  truncated: boolean;
  scopes: LogScopeInfo[];
}

export interface GetLogsOptions {
  limit?: number;
  /** Heuristic level filter: error | warn | info | debug. */
  level?: string;
  /** Case-insensitive substring filter over line text. */
  search?: string;
  /** Case-insensitive substring filter over line text and source. */
  query?: string;
  /** Epoch-ms lower bound (inclusive) on record timestamps. */
  sinceMs?: number;
  /** Epoch-ms upper bound (inclusive) on record timestamps. */
  untilMs?: number;
  /** Timestamp sort direction; default "desc" (newest first). */
  order?: "asc" | "desc";
  /** Filtered, ordered records to skip before the page (paging). */
  offset?: number;
}

/** get_logs — captured supervisor logs. `scope` is a process id or
 *  "all"; archived (previous-run) scope ids from the payload's scopes
 *  list are accepted as scopes too. `level` is an optional error/warn/
 *  info/debug heuristic filter; `search` and `query` are optional
 *  case-insensitive substrings (line text only / line text + source);
 *  `sinceMs`/`untilMs` bound record timestamps; results sort by
 *  timestamp per `order` (default "desc", newest first) and page via
 *  `offset` + `limit`. The payload is { lines, total_matching, total,
 *  truncated, scopes }; a bare-array reply (older backend) is
 *  normalized into the same shape. */
export async function getLogs(scope: string, opts: GetLogsOptions = {}): Promise<CallResult<LogPage>> {
  const empty: LogPage = { lines: [], total_matching: 0, total: 0, truncated: false, scopes: [] };
  if (!isTauri()) return { data: empty, live: false };
  try {
    const args: Record<string, unknown> = { scope, limit: opts.limit ?? 200 };
    if (opts.level) args.level = opts.level;
    if (opts.search) args.search = opts.search;
    if (opts.query) args.query = opts.query;
    if (opts.sinceMs !== undefined) args.sinceMs = opts.sinceMs;
    if (opts.untilMs !== undefined) args.untilMs = opts.untilMs;
    if (opts.order) args.order = opts.order;
    if (opts.offset !== undefined) args.offset = opts.offset;
    const raw = await invoke<unknown>("get_logs", args);
    if (Array.isArray(raw)) {
      // Pre-envelope backend: rows carry no source/archived markers.
      const lines = (raw as { ts: string; stream: string; line: string }[]).map((r) => ({
        ts: r.ts, stream: r.stream, line: r.line,
        source: scope === "all" ? "" : scope, archived: false,
      }));
      return { data: { lines, total_matching: lines.length, total: lines.length, truncated: false, scopes: [] }, live: true };
    }
    const page = (raw ?? {}) as Partial<LogPage>;
    const lines = (Array.isArray(page.lines) ? page.lines : []).map((r) => ({
      ts: r.ts, stream: r.stream, line: r.line,
      source: typeof r.source === "string" ? r.source : "",
      archived: Boolean(r.archived),
    }));
    const totalMatching = typeof page.total_matching === "number" ? page.total_matching : lines.length;
    return {
      data: {
        lines,
        total_matching: totalMatching,
        total: typeof page.total === "number" ? page.total : totalMatching,
        truncated: Boolean(page.truncated),
        scopes: Array.isArray(page.scopes) ? page.scopes : [],
      },
      live: true,
    };
  } catch (e) {
    return { data: empty, live: true, error: errOf(e) };
  }
}

/** diagnostics_export_bundle — write a diagnostics zip (config snapshot,
 *  per-scope recent logs, telemetry snapshot, versions) under the data
 *  dir; returns the bundle path. The logs inside are raw and unredacted —
 *  the view warns before sharing. */
export async function exportDiagnostics(): Promise<CallResult<{ path: string }>> {
  if (!isTauri()) return { data: { path: "" }, live: false, error: "diagnostics_export_bundle is not available in sample mode" };
  try {
    const data = await invoke<{ path: string }>("diagnostics_export_bundle");
    return { data, live: true };
  } catch (e) {
    return { data: { path: "" }, live: true, error: errOf(e) };
  }
}

/** open_folder — open a folder (or a file's containing folder) in
 *  Windows Explorer. Backs the "Open folder" button shown after a
 *  diagnostics export. */
export async function openFolder(path: string): Promise<CallResult<{ ok: boolean }>> {
  if (!isTauri()) return { data: { ok: false }, live: false, error: "open_folder is not available in sample mode" };
  try {
    const data = await invoke<{ ok: boolean }>("open_folder", { path });
    return { data, live: true };
  } catch (e) {
    return { data: { ok: false }, live: true, error: errOf(e) };
  }
}
