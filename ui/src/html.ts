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

/** A selector naming the focused control by its id or data attributes,
 *  so the same control can be found in a repainted container. */
function controlSelector(el: Element): string | null {
  if (el.id) return `#${CSS.escape(el.id)}`;
  const attrs = Array.from(el.attributes).filter((a) => a.name.startsWith("data-"));
  if (!attrs.length) return null;
  return el.tagName.toLowerCase() + attrs.map((a) => `[${a.name}="${CSS.escape(a.value)}"]`).join("");
}

/** Run a repaint of `root` and give the keyboard focus (and the text
 *  selection) back to the control that had it. */
export function keepFocus(root: Element, repaint: () => void): void {
  const el = document.activeElement;
  const sel = el && el !== document.body && root.contains(el) ? controlSelector(el) : null;
  const input = el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement ? el : null;
  let range: [number | null, number | null] | null = null;
  try { range = input ? [input.selectionStart, input.selectionEnd] : null; } catch { range = null; }
  repaint();
  if (!sel || el?.isConnected) return;
  const next = root.querySelector<HTMLElement>(sel);
  next?.focus({ preventScroll: true });
  if (range && range[0] !== null && (next instanceof HTMLInputElement || next instanceof HTMLTextAreaElement)) {
    try { next.setSelectionRange(range[0], range[1]); } catch { /* number and range inputs have no selection */ }
  }
}

/** True while the operator is in a field inside `root` whose repaint
 *  would lose an open dropdown, a drag or the caret. */
export function editingIn(root: Element | null): boolean {
  const el = document.activeElement;
  return Boolean(root && el && root.contains(el) && el.matches("input:not([type=checkbox]), select, textarea"));
}

/** Keep open popup menus inside their scroll container: a menu that
 *  would run past the bottom opens upwards, one past the left edge
 *  aligns to its left. */
export function fitMenus(root: Element): void {
  const box = (root.querySelector(".view") ?? root).getBoundingClientRect();
  root.querySelectorAll<HTMLElement>(".advanced-menu").forEach((m) => {
    m.classList.remove("up", "left");
    const r = m.getBoundingClientRect();
    if (r.bottom > box.bottom && r.height < (m.parentElement?.getBoundingClientRect().top ?? 0) - box.top) m.classList.add("up");
    if (r.left < box.left) m.classList.add("left");
  });
}
