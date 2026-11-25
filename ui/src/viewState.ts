/* Per-page UI state (filters, sort order, open tab) kept while the app
   runs: leaving a page and returning shows it as it was left. Nothing is
   written to disk. */

const states = new Map<string, unknown>();

/** The one state object of a page: `defaults` on the first visit, the
 *  same (mutated) object afterwards. */
export function pageState<T extends object>(key: string, defaults: T): T {
  const have = states.get(key) as T | undefined;
  if (have) return have;
  states.set(key, defaults);
  return defaults;
}
