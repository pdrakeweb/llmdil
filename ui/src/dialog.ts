/* In-app confirm/alert dialogs. The browser's own confirm()/alert() are
   titled with the webview's origin ("tauri.localhost says") and block
   the whole window, so no view calls them. */

import { esc } from "./html";
import { icon } from "./icons";

export interface DialogOpts {
  title: string;
  /** Plain text; line breaks are kept. */
  body?: string;
  confirmLabel?: string;
  cancelLabel?: string;
  /** A destructive action: the confirm button is red. */
  danger?: boolean;
}

function openDialog(o: DialogOpts, withCancel: boolean): Promise<boolean> {
  return new Promise((resolve) => {
    const prevFocus = document.activeElement as HTMLElement | null;
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";
    overlay.innerHTML = `<div class="modal-card" role="${withCancel ? "alertdialog" : "dialog"}" aria-modal="true" aria-label="${esc(o.title)}">
      <h3>${esc(o.title)}</h3>
      ${o.body ? `<p class="modal-sub" style="white-space:pre-line">${esc(o.body)}</p>` : ""}
      <div class="modal-actions">
        ${withCancel ? `<button type="button" class="btn" data-dialog="cancel">${icon("x")} ${esc(o.cancelLabel ?? "Cancel")}</button>` : ""}
        <button type="button" class="btn ${o.danger ? "danger" : "primary"}" data-dialog="ok">${icon("check")} ${esc(o.confirmLabel ?? "OK")}</button>
      </div></div>`;
    const buttons = (): HTMLElement[] => Array.from(overlay.querySelectorAll<HTMLElement>("[data-dialog]"));
    const onKey = (ev: KeyboardEvent): void => {
      if (ev.key === "Escape") { ev.preventDefault(); ev.stopPropagation(); finish(false); return; }
      if (ev.key !== "Tab") return;
      // Keep the focus inside the dialog.
      const list = buttons();
      const at = list.indexOf(document.activeElement as HTMLElement);
      ev.preventDefault();
      list[(at + (ev.shiftKey ? list.length - 1 : 1)) % list.length]?.focus();
    };
    const finish = (ok: boolean): void => {
      overlay.remove();
      document.removeEventListener("keydown", onKey, true);
      if (prevFocus?.isConnected) prevFocus.focus();
      resolve(ok);
    };
    overlay.addEventListener("click", (ev) => {
      if (ev.target === overlay) { finish(false); return; }
      const t = (ev.target as HTMLElement).closest("[data-dialog]");
      if (t) finish(t.getAttribute("data-dialog") === "ok");
    });
    document.addEventListener("keydown", onKey, true);
    document.body.appendChild(overlay);
    // A destructive dialog opens on Cancel so a stray Enter is harmless.
    (overlay.querySelector(`[data-dialog="${withCancel && o.danger ? "cancel" : "ok"}"]`) as HTMLElement | null)?.focus();
  });
}

/** Ask before an action; resolves true when the operator confirms. */
export function confirmDialog(o: DialogOpts): Promise<boolean> {
  return openDialog(o, true);
}

/** Tell the operator something that needs no decision. */
export async function alertDialog(title: string, body?: string): Promise<void> {
  await openDialog({ title, body }, false);
}
