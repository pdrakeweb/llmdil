/* Command errors in plain words: the Rust error enum's own prefixes
   ("invalid input:", "config error: validation failed:") and Tauri's
   argument-decoding message say nothing to the operator. */

const PREFIXES = /^(?:(?:error|invalid input|config error|validation failed|internal error):\s*)+/i;

const ARG_NAMES: Record<string, string> = { nCtx: "Context length", nBatch: "Batch size" };

/** "gatewayPort" -> "Gateway port". */
function argLabel(arg: string): string {
  if (ARG_NAMES[arg]) return ARG_NAMES[arg];
  const words = arg.replace(/_/g, " ").replace(/([a-z])([A-Z])/g, "$1 $2").toLowerCase();
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** Tauri's "invalid args `port` for command `backend_add`: invalid
 *  value: integer `70000`, expected u16", as a sentence. */
function plainArgError(msg: string): string | null {
  const m = /^invalid args `([^`]+)` for command `[^`]+`:\s*([\s\S]*)$/.exec(msg);
  if (!m) return null;
  const label = argLabel(m[1]);
  if (/missing required key|missing field/.test(m[2])) return `${label} is missing.`;
  if (/expected u16/.test(m[2])) return `${label} must be a whole number between 1 and 65,535.`;
  if (/expected (?:u32|u64|usize)/.test(m[2])) return `${label} must be a whole number, 0 or more.`;
  if (/expected (?:i32|i64|isize)/.test(m[2])) return `${label} must be a whole number.`;
  if (/expected f(?:32|64)/.test(m[2])) return `${label} must be a number.`;
  return `${label} is not a valid value.`;
}

export function plainError(msg: string): string {
  const arg = plainArgError(msg);
  if (arg) return arg;
  return msg.replace(PREFIXES, "") || msg;
}

/** Null when `n` is a usable TCP port, else what is wrong with it. */
export function portError(n: number): string | null {
  return Number.isInteger(n) && n >= 1 && n <= 65535 ? null : "Port must be a whole number between 1 and 65,535.";
}
