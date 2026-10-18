/* Inline SVG icon set — no external assets, no emojis.
   16x16 viewBox, stroke-based, currentColor. */

function svg(paths: string): string {
  return `<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${paths}</svg>`;
}

export const icons = {
  dashboard: () =>
    svg(`<rect x="1.5" y="1.5" width="5" height="5" rx="1"/><rect x="9.5" y="1.5" width="5" height="5" rx="1"/><rect x="1.5" y="9.5" width="5" height="5" rx="1"/><rect x="9.5" y="9.5" width="5" height="5" rx="1"/>`),
  map: () =>
    svg(`<circle cx="3" cy="3" r="1.6"/><circle cx="13" cy="3" r="1.6"/><circle cx="8" cy="13" r="1.6"/><path d="M4.4 4 6.8 11.6M11.6 4 9.2 11.6M4.6 3h6.8"/>`),
  gpu: () =>
    svg(`<rect x="4" y="4" width="8" height="8" rx="1"/><path d="M6.5 1.5v2M9.5 1.5v2M6.5 12.5v2M9.5 12.5v2M1.5 6.5h2M1.5 9.5h2M12.5 6.5h2M12.5 9.5h2"/>`),
  server: () =>
    svg(`<rect x="2" y="2.5" width="12" height="4.5" rx="1"/><rect x="2" y="9" width="12" height="4.5" rx="1"/><circle cx="4.5" cy="4.8" r="0.8" fill="currentColor"/><circle cx="4.5" cy="11.2" r="0.8" fill="currentColor"/>`),
  box: () =>
    svg(`<path d="M8 1.5 14 4.8v6.4L8 14.5 2 11.2V4.8z"/><path d="M2 4.8 8 8l6-3.2M8 8v6.5"/>`),
  logs: () =>
    svg(`<path d="M3 2.5h10M3 6h10M3 9.5h10M3 13h6"/>`),
  gateway: () =>
    svg(`<circle cx="8" cy="8" r="2"/><path d="M8 1.5v3M8 11.5v3M1.5 8h3M11.5 8h3"/>`),
  gear: () =>
    svg(`<circle cx="8" cy="8" r="2.2"/><path d="M8 1.5v2M8 12.5v2M1.5 8h2M12.5 8h2M3.4 3.4l1.4 1.4M11.2 11.2l1.4 1.4M12.6 3.4l-1.4 1.4M4.8 11.2 3.4 12.6"/>`),
  check: () =>
    svg(`<circle cx="8" cy="8" r="6.5"/><path d="m5.5 8.2 1.8 1.8 3.2-3.8"/>`),
  warn: () =>
    svg(`<path d="M8 1.8 14.5 13.5h-13z"/><path d="M8 6v3.4M8 11.6v.2"/>`),
  err: () =>
    svg(`<circle cx="8" cy="8" r="6.5"/><path d="M6 6l4 4M10 6l-4 4"/>`),
  restart: () =>
    svg(`<path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9M13.5 1.8v3h-3"/>`),
  shield: () =>
    svg(`<path d="M8 1.5 13 3.5v4c0 3.2-2.1 5.4-5 7-2.9-1.6-5-3.8-5-7v-4z"/>`),
  key: () =>
    svg(`<circle cx="5.2" cy="10.8" r="3.1"/><path d="M7.5 8.5 13.8 2.2M11 5.2l2.1.4M9.2 7l1.9.5"/>`),
  bolt: () =>
    svg(`<path d="M9 1.5 3.5 9H7l-1 5.5L11.5 7H8z"/>`),
  layers: () =>
    svg(`<path d="m8 1.5 6 3-6 3-6-3z"/><path d="m2 7.5 6 3 6-3M2 10.5l6 3 6-3"/>`),
  search: () =>
    svg(`<circle cx="7" cy="7" r="4.5"/><path d="m10.5 10.5 3.5 3.5"/>`),
  download: () =>
    svg(`<path d="M8 1.5v8M4.8 6.8 8 10l3.2-3.2M2.5 12.5h11"/>`),
  clock: () =>
    svg(`<circle cx="8" cy="8" r="6.5"/><path d="M8 4.5V8l2.5 1.5"/>`),
  calendar: () =>
    svg(`<rect x="2" y="3" width="12" height="11" rx="1.5"/><path d="M2 6.5h12M5.5 1.8V4M10.5 1.8V4"/>`),
  wrench: () =>
    svg(`<path d="M10.5 2.5a3.2 3.2 0 0 0-4.3 4L2 10.7 5.3 14l4.2-4.2a3.2 3.2 0 0 0 4-4.3l-2 2-2.2-.6-.6-2.2z"/>`),
  pause: () => svg(`<path d="M6 3.5v9M10 3.5v9"/>`),
  play: () => svg(`<path d="M5 3.5v9l7-4.5z"/>`),
  menu: () => svg(`<path d="M2.5 4h11M2.5 8h11M2.5 12h11"/>`),
  info: () => svg(`<circle cx="8" cy="8" r="6.5"/><path d="M8 7.4v3.4M8 4.8v.2"/>`),
  ban: () => svg(`<circle cx="8" cy="8" r="6.2"/><path d="m3.9 3.9 8.2 8.2"/>`),
  bell: () =>
    svg(`<path d="M8 2.2a4.3 4.3 0 0 1 4.3 4.3c0 3 1 3.9 1.6 4.7H2.1c.6-.8 1.6-1.7 1.6-4.7A4.3 4.3 0 0 1 8 2.2ZM6.6 13.4a1.5 1.5 0 0 0 2.8 0"/>`),
  trash: () =>
    svg(`<path d="M3 4h10M6.5 4V2.8h3V4M4.5 4l.7 9.2h5.6L11.5 4M6.8 6.8v4M9.2 6.8v4"/>`),
  copy: () =>
    svg(`<rect x="5.5" y="5.5" width="8" height="8" rx="1.5"/><path d="M10.5 5.5v-1a2 2 0 0 0-2-2h-4a2 2 0 0 0-2 2v4a2 2 0 0 0 2 2h1"/>`),
  eye: () =>
    svg(`<path d="M1.8 8S4.1 4.2 8 4.2 14.2 8 14.2 8 11.9 11.8 8 11.8 1.8 8 1.8 8z"/><circle cx="8" cy="8" r="2"/>`),
  eyeOff: () =>
    svg(`<path d="M5.6 5.9C3.4 6.9 1.8 8 1.8 8s2.3 3.8 6.2 3.8c1.2 0 2.3-.3 3.2-.8M10.4 5.9c2.2 1 3.8 2.1 3.8 2.1a17.4 17.4 0 0 1-1.9 2.1M6.3 6.7A2 2 0 0 0 8 10a2 2 0 0 0 1.3-.5"/><path d="M2.5 2.5l11 11"/>`),
  x: () => svg(`<path d="M4 4l8 8M12 4l-8 8"/>`),
  target: () =>
    svg(`<circle cx="8" cy="8" r="6"/><circle cx="8" cy="8" r="2.4"/>`),
  plus: () => svg(`<path d="M8 3v10M3 8h10"/>`),
  chevL: () => svg(`<path d="m10 3.5L5.5 8 10 12.5"/>`),
  chevR: () => svg(`<path d="m6 3.5L10.5 8 6 12.5"/>`),
  chevD: () => svg(`<path d="m3.5 6 4.5 4.5L12.5 6"/>`),
  pencil: () =>
    svg(`<path d="M11.6 2.6a1.8 1.8 0 0 1 2.5 0l.3.3a1.8 1.8 0 0 1 0 2.5l-8.2 8.2-3.5.9.9-3.5z"/>`),
  folder: () =>
    svg(`<path d="M2 4.5A1.5 1.5 0 0 1 3.5 3h3L8 5h4.5A1.5 1.5 0 0 1 14 6.5v5A1.5 1.5 0 0 1 12.5 13h-9A1.5 1.5 0 0 1 2 11.5z"/>`),
  external: () =>
    svg(`<path d="M6.5 3.5H4A1.5 1.5 0 0 0 2.5 5v7A1.5 1.5 0 0 0 4 13.5h7A1.5 1.5 0 0 0 12.5 12V9.5M9.5 2.5h4v4M13.2 2.8 7.8 8.2"/>`),
  peak: () =>
    svg(`<path d="M2 12.5 6.2 5l2.3 3.4L10.4 6 14 12.5z"/>`),
};

export type IconName = keyof typeof icons;

export function icon(name: IconName): string {
  return icons[name]();
}
