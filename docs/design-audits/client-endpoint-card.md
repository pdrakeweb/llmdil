# Design audit — Client endpoint card

Card: Client endpoint (Gateway view) — inline in `gatewayPaint`, `ui/src/views/static.ts`.
Audited against: `docs/design-principles.md` (14 principles, Part 3 checklist).
Audit date: 2026-10-16. Line numbers are as of the audit read. The file is under concurrent edit by the in-flight de-wordify + copy-button pass: the endpoint block shifted +39 lines between two audit reads (edits above it), with its internal structure identical in both reads. Findings cite the block's current lines (2908–2931) and the click wiring in `wireGateway` (3133–3172).

## Scorecard

Every checklist answer must be "no" unless noted. F = fail.

| # | Question (short) | Result | Evidence |
|---|---|---|---|
| 1 | Compact | Pass | Card is one short block: title, 4 value rows, one action row (`static.ts:2908–2931`). Nothing in it needs a tab, toggle, or expandable. |
| 2 | One section, multiple views | Pass | Base URL (loopback, :2910) and LAN base URL (:2919) are distinct values for distinct client locations, each rendered once. No data is presented two ways. |
| 3 | Prime real estate | Pass | After the title, the first content row is the card's answer — Base URL (:2910). |
| 4 | Narrative in tooltips | **F** | Three prose strings still render inline: "Reachable from this machine only." (:2917); "Bound to all interfaces; this machine's LAN address could not be determined." (:2921); the master-key value slot renders the instructional prose "not loaded — Reveal loads it" (:2922). |
| 5 | Real data | Pass | The card contains no bars, fills, gauges, segments, or data-encoding colors. Nothing to falsify. |
| 6 | Numbers in one place | Pass | The only number is the port, and it appears only inside its own distinct URL value (:2910, :2919). No number is repeated as tile + line or chart + text. |
| 7 | Density with order | **F** (minor) | The `card-actions` row (:2926–2931) — Restart/Start gateway, Stop gateway, Setup Wizard, LiteLLM admin UI — answers "how do I control the gateway", grouped inside a card whose question is "what do clients connect to". Lifecycle controls are mixed into the connection card. |
| 8 | Multi-number data is a chart | Pass (n/a) | Every value is a single value (two URLs, one key, one bind control). No related number set exists to chart. |
| 9 | Label: Value | **F** | :2917 and :2921 are bare sentences with no label. At :2922 the label is a proper noun ("Master key:") but the value position holds a sentence fragment ("not loaded — Reveal loads it") instead of a value. Compliant rows for contrast: "Base URL: …" (:2910), "Network access: …" (:2912), "LAN base URL: …" (:2919). |
| 10 | Copy buttons | Pass, 1 observation | Every rendered URL/key has a working copy button: Base URL (:2911, handler :3145), LAN base URL (:2920, handler :3155, null-guarded), Master key (:2924, handler :3133 — fetches the key on demand, then writes it). Observation: the admin UI address (`http://127.0.0.1:<port>/ui`) is never rendered as a value — it exists only inside the button handler (:3166) — so there is nothing to attach a copy button to. |
| 11 | Status colors | Pass | The card renders no status chips and no chip stating a default state. |
| 12 | Unknown is not zero | Pass | No unknown renders as 0 or as a fabricated measurement. A loaded-but-hidden key is masked with bullets (:2922), which is masking, not a fake value. The unknown states (unloaded key, undetermined LAN address) render prose — that wording fails Q4/Q9/Q13, not Q12. |
| 13 | Empty states | **F** (minor) | When bind is loopback, the LAN block has no URL to show yet renders a prose line (:2917) instead of being omitted; same for the undetermined-LAN branch (:2921). Per principle 13, both branches should render nothing. |
| 14 | Number formatting | Pass | Integers only (ports inside URLs). No decimals anywhere in the card. |

**Result: 4 of 14 questions fail (Q4, Q7, Q9, Q13), at 4 code sites, plus 1 observation (Q10).**

## What the in-flight pass already closed

The de-wordify + copy-button pass in flight has landed in this card. Against Pete's original paste of this view, it closed:

- Q4/Q9 for all four long narratives: LAN note → info-tip on the LAN row (:2919); master-key explanation → info-tip on the Master key label (:2922); start-gateway step description → info-tip on the Restart button (:2927); admin UI sign-in text → info-tip on the admin UI button (:2930).
- Q10 in full for rendered values: Copy buttons with working, failure-reporting clipboard handlers for Base URL, LAN base URL, and Master key (handlers at :3133, :3145, :3155).

It does **not** close the residual gaps:

- :2917 and :2921 — the two conditional LAN branches were not in Pete's paste (he runs LAN bind with a determined address, so neither branch renders for him), and the pass's tooltip list did not include them.
- :2922 value prose — the pass moved the separate narrative paragraph into the info-tip but left "not loaded — Reveal loads it" in the value slot. This string *was* in Pete's paste.
- Q7 action-row grouping — outside the pass's stated scope.
- Q13 — same two conditional branches as Q4.

## Proposed redesign

Skeleton unchanged; the card becomes values-only. Only change 4 is structural, and it is optional.

```
Client endpoint
Base URL: http://127.0.0.1:4000/v1                 [Copy]
Network access: [Local network (all interfaces) ▾] (i)
LAN base URL: http://192.168.5.23:4000/v1          [Copy]   (row only when a LAN URL exists)
Master key: ••••••••••••                           [Reveal] [Copy]
```

1. **Network access absorbs both conditional sentences.** Add an info-tip beside the Network access label carrying whichever explanation applies: loopback → "Reachable from this machine only."; all-interfaces with no determined LAN address → "Bound to all interfaces; this machine's LAN address could not be determined." Delete the standalone prose rows (:2917, :2921). The LAN row renders only when a LAN URL exists. Closes Q4, Q9, Q13 for both branches.
2. **Master key value states.** Loaded + hidden → bullets (unchanged); revealed → the key (unchanged); not loaded → the value slot shows "not loaded" and "Reveal loads it." moves into the existing info-tip as an appended sentence. Closes Q9 (and the Q4 fragment) at :2922.
3. **Copy buttons (principle 10) — final inventory.** Base URL: existing (:2911). LAN base URL: existing (:2920). Master key: existing (:2924). No additions needed. The admin UI stays a button that opens the UI, its narrative already in its info-tip; if a future change renders the `/ui` address as text, it gets a Copy button at that point.
4. **Optional: move the action row (Q7).** Relocate the `card-actions` row (:2926–2931) out of the endpoint card to directly under the gateway summary card. Button ids and handlers are unchanged — the handlers bind by id within the view — so this is a markup relocation inside `gatewayPaint`'s return string, with no wiring changes. The endpoint card then holds exactly: title, Base URL, Network access, LAN base URL (when applicable), Master key.
