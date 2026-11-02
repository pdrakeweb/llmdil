/* Start-gateway MCP credential prompt (Pete, 2026-10-18).
   Before an interactive gateway start, the Gateway-exposed MCP
   servers the start would omit for want of a credential are
   offered registration and storage in place: a link button to
   the service's signup / API-key page, a credential field with a
   Store button (the mcp_store_credential command — the same
   storage flow the MCP Servers view uses), and a Skip.
   Storing re-arms the server for THIS start: the prompt runs
   before the start-gateway step, and the step resolves arming
   when it runs, so a stored credential is picked up by the
   config the start writes. Skipping — or a failed store —
   leaves the server out, which is the start's standing
   behavior. Only the interactive start paths call this (the
   Gateway view's Start button, the dashboard's Start all);
   non-interactive starts (service mode, supervision, the REST
   activation apply) never prompt — they omit unarmed servers
   silently, as before. */

import { mcpStartPreflight, openUrl, storeMcpCredential } from "./api";
import type { McpStartPreflightServer } from "./api";
import { esc } from "./html";
import { icon } from "./icons";

function rowHtml(s: McpStartPreflightServer): string {
  return `<div class="mcp-prompt-row" data-prompt-row="${esc(s.id)}">
    <div class="mcp-prompt-head"><strong>${esc(s.display_name)}</strong><span class="mcp-prompt-cred" data-prompt-cred>Credential: missing</span></div>
    <div class="mcp-prompt-line">The gateway starts without this server until its credential is stored.${s.metered ? " Paid API." : ""}</div>
    ${s.credential_hint ? `<div class="mcp-prompt-line">Credential: ${esc(s.credential_hint)}</div>` : ""}
    <div class="mcp-prompt-controls">
      ${s.signup_url ? `<button class="btn" data-prompt-act="link" data-prompt-id="${esc(s.id)}">${icon("external")} Get an API key</button>` : ""}
      <input type="password" data-prompt-input placeholder="${esc(s.env_keys[0] ?? "API key")}" autocomplete="off" aria-label="Credential value for ${esc(s.display_name)}">
      <button class="btn" data-prompt-act="store" data-prompt-id="${esc(s.id)}">${icon("shield")} Store credential</button>
      <button class="btn" data-prompt-act="skip" data-prompt-id="${esc(s.id)}">Skip</button>
    </div>
    <div class="mcp-prompt-msg" data-prompt-msg></div>
  </div>`;
}

/** Ask about unarmed Gateway MCP servers before a gateway
 *  start. Resolves true when the start should proceed (nothing
 *  missing, every server stored or skipped, or the operator
 *  chose Continue), false when the operator cancelled the
 *  start. A preflight failure resolves true: the prompt must
 *  never be the reason a start cannot run — the start itself
 *  reports unarmed servers in its step outcome, as before. */
export async function promptForUnarmedMcpCredentials(): Promise<boolean> {
  let servers: McpStartPreflightServer[] = [];
  try {
    const r = await mcpStartPreflight();
    if (r.error || !r.live) return true;
    servers = r.data.servers;
  } catch {
    return true;
  }
  if (!servers.length) return true;

  return new Promise<boolean>((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";
    overlay.innerHTML = `<div class="modal-card" role="dialog" aria-modal="true" aria-label="MCP server credentials">
      <h3>${icon("key")} MCP server credentials</h3>
      <p class="modal-sub">These servers are registered with the gateway but have no stored credential, so the gateway starts without them. Store a credential now to include its server in this start.</p>
      ${servers.map(rowHtml).join("")}
      <div class="modal-actions">
        <button class="btn" data-prompt-act="cancel">Cancel start</button>
        <button class="btn primary" data-prompt-act="continue">${icon("play")} Continue start</button>
      </div>
    </div>`;
    document.body.appendChild(overlay);

    const finish = (proceed: boolean): void => {
      overlay.remove();
      document.removeEventListener("keydown", onKey, true);
      resolve(proceed);
    };
    const onKey = (ev: KeyboardEvent): void => {
      if (ev.key === "Escape") {
        ev.preventDefault();
        finish(false);
      }
    };
    document.addEventListener("keydown", onKey, true);

    const setMsg = (row: HTMLElement, text: string, err: boolean): void => {
      const msg = row.querySelector("[data-prompt-msg]") as HTMLElement | null;
      if (msg) {
        msg.textContent = text;
        msg.style.color = err ? "var(--err)" : "var(--text2)";
      }
    };
    /** A resolved row (stored or skipped): its controls lock and
     *  the Credential line states the outcome. */
    const resolveRow = (row: HTMLElement, credText: string, msgText: string): void => {
      const cred = row.querySelector("[data-prompt-cred]") as HTMLElement | null;
      if (cred) cred.textContent = credText;
      row.querySelectorAll("input, button").forEach((el) => {
        (el as HTMLInputElement | HTMLButtonElement).disabled = true;
      });
      setMsg(row, msgText, false);
    };

    overlay.addEventListener("click", (ev) => {
      void (async (): Promise<void> => {
        const t = (ev.target as HTMLElement).closest("[data-prompt-act]") as HTMLElement | null;
        if (!t || (t as HTMLButtonElement).disabled) return;
        const act = t.getAttribute("data-prompt-act") ?? "";
        if (act === "cancel") { finish(false); return; }
        if (act === "continue") { finish(true); return; }
        const id = t.getAttribute("data-prompt-id") ?? "";
        const server = servers.find((s) => s.id === id);
        if (!server) return;
        // Server ids are validated ^[a-z][a-z0-9_]*$ at save, so
        // the attribute selector needs no escaping.
        const row = overlay.querySelector(`[data-prompt-row="${id}"]`) as HTMLElement | null;
        if (!row) return;
        if (act === "link") {
          if (server.signup_url) await openUrl(server.signup_url);
          return;
        }
        if (act === "skip") {
          resolveRow(row, "Credential: skipped", "Skipped — the gateway starts without this server.");
          return;
        }
        if (act === "store") {
          const input = row.querySelector("[data-prompt-input]") as HTMLInputElement | null;
          const value = input?.value ?? "";
          if (!value.trim()) {
            setMsg(row, "Enter the credential value first.", true);
            return;
          }
          const btn = t as HTMLButtonElement;
          btn.disabled = true;
          const r = await storeMcpCredential(id, value);
          // The value is cleared and never rendered back,
          // whatever the outcome.
          if (input) input.value = "";
          btn.disabled = false;
          if (r.error || !r.data.stored) {
            setMsg(row, `Credential not stored: ${r.error ?? "the store command did not confirm the store."}`, true);
            return;
          }
          resolveRow(row, "Credential: stored", "Stored — this server is included in this start.");
        }
      })();
    });
  });
}
