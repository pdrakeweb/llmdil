/* HTML escaping for values interpolated into innerHTML.
   Backend- and mock-provided strings (names, model ids, event text, process
   names) must never be injected raw — always pass them through esc(). */

const ESCAPE_MAP: Record<string, string> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
  '"': "&quot;",
  "'": "&#39;",
};

export function esc(value: unknown): string {
  return String(value).replace(/[&<>"']/g, (ch) => ESCAPE_MAP[ch]);
}
