import { createSignal } from "solid-js";

// One clock for every countdown on the page.
const [nowSig, setNow] = createSignal(Date.now());
setInterval(() => setNow(Date.now()), 1000);
export const now = nowSig;

export function ago(ts: number, n = now()): string {
  const s = Math.max(0, Math.floor((n - ts) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

export function until(ts?: number | null, n = now()): string {
  if (!ts) return "–";
  const s = Math.ceil((ts - n) / 1000);
  if (s <= 0) return "now";
  if (s >= 86400) return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
  if (s >= 3600) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  if (s >= 60) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${s}s`;
}

export const money = (v: number) => (v === 0 ? "$0" : v < 0.01 ? `$${v.toFixed(4)}` : `$${v.toFixed(2)}`);
export const num = (v: number) => (v >= 1e6 ? `${(v / 1e6).toFixed(1)}M` : v >= 1e4 ? `${Math.round(v / 1e3)}k` : v >= 1e3 ? `${(v / 1e3).toFixed(1)}k` : `${v}`);
export const bytes = (v?: number | null) => (v == null ? "–" : v >= 1 << 30 ? `${(v / (1 << 30)).toFixed(1)} GiB` : v >= 1 << 20 ? `${(v / (1 << 20)).toFixed(0)} MiB` : v >= 1024 ? `${(v / 1024).toFixed(0)} KiB` : `${v} B`);
export const clock = (ts: number) => new Date(ts).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
export const short = (id: string) => (id.startsWith("art://b3/") ? `art://b3/${id.slice(9, 21)}` : id);

/** A stable hue per node name, the same in both themes (PHASE1_5 §13b). */
export function nodeHue(name: string): number {
  let h = 2166136261;
  for (const c of name) h = Math.imul(h ^ c.charCodeAt(0), 16777619);
  // Skip the red/amber band so a node colour is never read as a status.
  const hue = (h >>> 0) % 260;
  return hue < 20 ? hue + 160 : hue + 70 > 360 ? hue : hue + 70;
}

export const todoProgress = (t: { status: string }[]) => (t.length ? `${t.filter((x) => x.status === "completed").length}/${t.length}` : "–");

export function cx(...c: (string | false | null | undefined)[]): string {
  return c.filter(Boolean).join(" ");
}
