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

/* The last HTML each element was given through setHtml. Keyed by the
   element itself, so a container rebuilt by an outer repaint starts
   fresh. */
const lastHtml = new WeakMap<Element, string>();

/** Set `el.innerHTML` only when `html` differs from what setHtml last
 *  put there: an identical repaint (a store poll that changed nothing
 *  visible) must not replace the nodes under the pointer — a button
 *  replaced between mousedown and mouseup loses its click, and open
 *  tooltips and focus are dropped. Returns whether it repainted. Every
 *  write to `el` must go through here for the comparison to hold. */
export function setHtml(el: Element, html: string): boolean {
  if (lastHtml.get(el) === html) return false;
  el.innerHTML = html;
  lastHtml.set(el, html);
  return true;
}
