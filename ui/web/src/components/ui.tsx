// Small UI kit. Every status is shown with colour AND an icon AND a word
// (PHASE1_5 §13b), so nothing depends on telling colours apart.
import { Dialog as KDialog } from "@kobalte/core/dialog";
import { Toast, toaster } from "@kobalte/core/toast";
import { For, JSX, Show, createSignal, splitProps } from "solid-js";
import { Dynamic, Portal } from "solid-js/web";
import * as I from "../icons";
import type { State, Todo } from "../types";
import { cx, nodeHue, until, now } from "../util";

export function Button(p: JSX.ButtonHTMLAttributes<HTMLButtonElement> & { variant?: "primary" | "ghost" | "danger" | "default"; size?: "sm" | "md"; busy?: boolean }) {
  const [l, rest] = splitProps(p, ["variant", "size", "class", "busy", "children", "disabled"]);
  const v = () =>
    ({
      primary: "bg-indigo-600 text-white hover:bg-indigo-500 focus-visible:outline-indigo-500 disabled:bg-indigo-600/50",
      danger: "bg-red-600 text-white hover:bg-red-500 focus-visible:outline-red-500 disabled:bg-red-600/50",
      ghost: "hover:bg-slate-200/70 dark:hover:bg-slate-800 text-slate-700 dark:text-slate-300",
      default: "bg-white ring-1 ring-slate-300 hover:bg-slate-50 dark:bg-slate-900 dark:ring-slate-700 dark:hover:bg-slate-800",
    })[l.variant ?? "default"];
  return (
    <button
      {...rest}
      disabled={l.disabled || l.busy}
      class={cx(
        "inline-flex items-center justify-center gap-1.5 rounded-lg font-medium transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 disabled:cursor-not-allowed disabled:opacity-60",
        l.size === "sm" ? "px-2.5 py-1 text-xs" : "px-3.5 py-2 text-sm",
        v(),
        l.class,
      )}
    >
      <Show when={l.busy}>
        <I.Loader class="size-4 animate-spin" />
      </Show>
      {l.children}
    </button>
  );
}

export function Card(p: { children: JSX.Element; class?: string; accent?: string; striped?: boolean; onClick?: () => void }) {
  return (
    <div
      onClick={p.onClick}
      class={cx(
        "rounded-xl bg-white ring-1 ring-slate-200 shadow-sm dark:bg-slate-900 dark:ring-slate-800",
        p.accent && "border-l-4",
        p.striped && "bg-[repeating-linear-gradient(135deg,transparent,transparent_10px,rgba(239,68,68,0.07)_10px,rgba(239,68,68,0.07)_20px)]",
        p.onClick && "cursor-pointer hover:ring-slate-300 dark:hover:ring-slate-700",
        p.class,
      )}
      style={p.accent ? { "border-left-color": p.accent } : undefined}
    >
      {p.children}
    </div>
  );
}

export function PageHeader(p: { title: string; subtitle?: string; actions?: JSX.Element }) {
  return (
    <div class="mb-4 flex flex-wrap items-end justify-between gap-3">
      <div>
        <h1 class="text-xl font-semibold tracking-tight">{p.title}</h1>
        <Show when={p.subtitle}>
          <p class="text-sm text-slate-500 dark:text-slate-400">{p.subtitle}</p>
        </Show>
      </div>
      <div class="flex flex-wrap gap-2">{p.actions}</div>
    </div>
  );
}

const STATUS: Record<State, { label: string; icon: any; cls: string }> = {
  running: { label: "running", icon: I.Activity, cls: "bg-emerald-100 text-emerald-800 ring-emerald-300 dark:bg-emerald-950 dark:text-emerald-300 dark:ring-emerald-800" },
  idle: { label: "idle", icon: I.Moon, cls: "bg-slate-100 text-slate-700 ring-slate-300 dark:bg-slate-800 dark:text-slate-300 dark:ring-slate-700" },
  blocked: { label: "waiting", icon: I.CircleHelp, cls: "bg-amber-100 text-amber-900 ring-amber-300 dark:bg-amber-950 dark:text-amber-300 dark:ring-amber-800" },
  parked: { label: "parked", icon: I.Clock, cls: "bg-sky-100 text-sky-900 ring-sky-300 dark:bg-sky-950 dark:text-sky-300 dark:ring-sky-800" },
  error: { label: "error", icon: I.AlertTriangle, cls: "bg-red-100 text-red-800 ring-red-300 dark:bg-red-950 dark:text-red-300 dark:ring-red-800" },
  dead: { label: "dead", icon: I.XCircle, cls: "bg-red-600 text-white ring-red-700 dark:bg-red-700 dark:ring-red-500" },
  halted: { label: "halted", icon: I.PauseCircle, cls: "bg-orange-100 text-orange-900 ring-orange-300 dark:bg-orange-950 dark:text-orange-300 dark:ring-orange-800" },
  stopped: { label: "stopped", icon: I.Square, cls: "bg-slate-100 text-slate-500 ring-slate-200 dark:bg-slate-900 dark:text-slate-500 dark:ring-slate-800" },
  starting: { label: "starting", icon: I.Loader, cls: "bg-indigo-100 text-indigo-800 ring-indigo-300 dark:bg-indigo-950 dark:text-indigo-300 dark:ring-indigo-800" },
  unreachable: { label: "unreachable", icon: I.WifiOff, cls: "bg-zinc-200 text-zinc-800 ring-zinc-300 dark:bg-zinc-800 dark:text-zinc-300 dark:ring-zinc-700" },
  lost: { label: "lost", icon: I.Ghost, cls: "bg-zinc-200 text-zinc-800 ring-zinc-400 dark:bg-zinc-800 dark:text-zinc-300 dark:ring-zinc-600" },
};

export function StatusBadge(p: { state: State; questions?: number }) {
  const s = () => STATUS[p.state] ?? STATUS.idle;
  return (
    <span class={cx("inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-xs font-medium ring-1", s().cls)} title={p.state}>
      <Show when={p.state === "running"} fallback={<Dynamic component={s().icon} class={cx("size-3.5", p.state === "starting" && "animate-spin")} />}>
        <span class="relative flex size-2">
          <span class="absolute inline-flex h-full w-full animate-ping rounded-full bg-emerald-500 opacity-60" />
          <span class="relative inline-flex size-2 rounded-full bg-emerald-500" />
        </span>
      </Show>
      {s().label}
      <Show when={(p.questions ?? 0) > 0}>
        <span class="ml-0.5 rounded-full bg-amber-500 px-1 text-[10px] font-bold text-white">?{p.questions}</span>
      </Show>
    </span>
  );
}

export const nodeColor = (name: string) => `hsl(${nodeHue(name)} 65% 50%)`;

export function NodeChip(p: { name: string; relay?: boolean }) {
  return (
    <span
      class={cx("inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 font-mono text-xs", p.relay ? "ring-1 ring-dashed" : "text-white")}
      style={p.relay ? { color: nodeColor(p.name), "--tw-ring-color": nodeColor(p.name) } : { "background-color": `hsl(${nodeHue(p.name)} 55% 38%)` }}
      title={p.relay ? `relay node ${p.name}` : `node ${p.name}`}
    >
      <I.Server class="size-3" />
      {p.name}
    </span>
  );
}

export function IpChip(p: { ip?: string | null; sharers?: number }) {
  return (
    <span class="inline-flex items-center gap-1 rounded-md bg-slate-100 px-1.5 py-0.5 font-mono text-xs dark:bg-slate-800" title={p.sharers && p.sharers > 1 ? `shared by ${p.sharers}` : "egress IP"}>
      <Show when={(p.sharers ?? 0) > 1}>
        <I.Link class="size-3" />
      </Show>
      {p.ip ?? "?"}
      <Show when={(p.sharers ?? 0) > 1}>
        <span class="text-slate-500">×{p.sharers}</span>
      </Show>
    </span>
  );
}

/** green → amber at 70% → red at 90%, with the reset countdown. */
export function QuotaBar(p: { used: number; limit?: number | null; remaining?: number | null; exhausted: boolean; resets?: number | null }) {
  const pct = () => {
    if (p.exhausted) return 100;
    const lim = p.limit ?? (p.remaining != null ? p.used + p.remaining : null);
    return lim ? Math.min(100, (p.used / Math.max(1, lim)) * 100) : 0;
  };
  const color = () => (pct() >= 90 ? "bg-red-500" : pct() >= 70 ? "bg-amber-500" : "bg-emerald-500");
  return (
    <div class="flex items-center gap-2">
      <div class="h-2 flex-1 overflow-hidden rounded-full bg-slate-200 dark:bg-slate-800" role="progressbar" aria-valuenow={Math.round(pct())} aria-valuemin={0} aria-valuemax={100}>
        <div class={cx("h-full rounded-full transition-all", color())} style={{ width: `${pct()}%` }} />
      </div>
      <span class="w-28 shrink-0 text-right font-mono text-xs tabular-nums">
        {p.exhausted ? <span class="font-semibold text-red-600 dark:text-red-400">used up</span> : `${p.used}/${p.limit ?? "?"}`}
      </span>
      <span class="inline-flex w-20 shrink-0 items-center gap-1 text-xs text-slate-500" title="resets in">
        <I.Clock class="size-3" />
        {(now(), until(p.resets))}
      </span>
    </div>
  );
}

export function Pill(p: { children: JSX.Element; tone?: "slate" | "amber" | "red" | "emerald" | "sky" | "indigo" | "orange"; title?: string }) {
  const t = () =>
    ({
      slate: "bg-slate-100 text-slate-700 dark:bg-slate-800 dark:text-slate-300",
      amber: "bg-amber-100 text-amber-900 dark:bg-amber-950 dark:text-amber-300",
      red: "bg-red-100 text-red-800 dark:bg-red-950 dark:text-red-300",
      emerald: "bg-emerald-100 text-emerald-800 dark:bg-emerald-950 dark:text-emerald-300",
      sky: "bg-sky-100 text-sky-900 dark:bg-sky-950 dark:text-sky-300",
      indigo: "bg-indigo-100 text-indigo-800 dark:bg-indigo-950 dark:text-indigo-300",
      orange: "bg-orange-100 text-orange-900 dark:bg-orange-950 dark:text-orange-300",
    })[p.tone ?? "slate"];
  return (
    <span class={cx("inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 text-xs font-medium", t())} title={p.title}>
      {p.children}
    </span>
  );
}

export function Importance(p: { level: string }) {
  return (
    <Show
      when={p.level === "vital"}
      fallback={
        <Pill tone={p.level === "normal" ? "amber" : "slate"}>
          <I.CircleHelp class="size-3" />
          {p.level}
        </Pill>
      }
    >
      <Pill tone="red">
        <I.AlertTriangle class="size-3" />
        vital
      </Pill>
    </Show>
  );
}

export function Todos(p: { todos: Todo[] }) {
  const mark = (s: string) => (s === "completed" ? "✓" : s === "in_progress" ? "▶" : s === "cancelled" ? "–" : "○");
  return (
    <Show when={p.todos.length} fallback={<p class="text-sm text-slate-500">No todo list yet.</p>}>
      <ul class="space-y-1 text-sm">
        <For each={p.todos}>
          {(t) => (
            <li class={cx("flex gap-2", t.status === "completed" && "text-slate-500 line-through", t.status === "in_progress" && "font-medium")}>
              <span class={cx("w-4 shrink-0 text-center", t.status === "in_progress" && "text-emerald-600", t.status === "cancelled" && "text-red-500")} aria-label={t.status}>
                {mark(t.status)}
              </span>
              {t.content}
            </li>
          )}
        </For>
      </ul>
    </Show>
  );
}

export function Empty(p: { icon?: any; title: string; children?: JSX.Element }) {
  return (
    <div class="rounded-xl border border-dashed border-slate-300 p-8 text-center dark:border-slate-700">
      <Dynamic component={p.icon ?? I.Boxes} class="mx-auto mb-2 size-8 text-slate-400" />
      <p class="font-medium">{p.title}</p>
      <div class="mt-1 text-sm text-slate-500">{p.children}</div>
    </div>
  );
}

export function Field(p: { label: string; hint?: string; children: JSX.Element }) {
  return (
    <label class="block space-y-1">
      <span class="text-sm font-medium">{p.label}</span>
      {p.children}
      <Show when={p.hint}>
        <span class="block text-xs text-slate-500">{p.hint}</span>
      </Show>
    </label>
  );
}

export const inputCls =
  "w-full rounded-lg bg-white px-3 py-2 text-sm ring-1 ring-slate-300 placeholder:text-slate-400 focus:outline-none focus:ring-2 focus:ring-indigo-500 dark:bg-slate-950 dark:ring-slate-700";

export function Modal(p: { open: boolean; onClose: () => void; title: string; children: JSX.Element; wide?: boolean }) {
  return (
    <KDialog open={p.open} onOpenChange={(o) => !o && p.onClose()}>
      <KDialog.Portal>
        <KDialog.Overlay class="fixed inset-0 z-40 bg-slate-950/50 backdrop-blur-[1px]" />
        <div class="fixed inset-0 z-50 flex items-end justify-center sm:items-center sm:p-4">
          <KDialog.Content class={cx("max-h-[90vh] w-full overflow-y-auto rounded-t-2xl bg-white p-5 shadow-xl ring-1 ring-slate-200 sm:rounded-2xl dark:bg-slate-900 dark:ring-slate-800", p.wide ? "sm:max-w-2xl" : "sm:max-w-lg")}>
            <div class="mb-4 flex items-center justify-between">
              <KDialog.Title class="text-lg font-semibold">{p.title}</KDialog.Title>
              <KDialog.CloseButton class="rounded-md p-1 hover:bg-slate-100 dark:hover:bg-slate-800" aria-label="close">
                <I.X class="size-5" />
              </KDialog.CloseButton>
            </div>
            {p.children}
          </KDialog.Content>
        </div>
      </KDialog.Portal>
    </KDialog>
  );
}

export function notify(title: string, body = "", tone: "ok" | "error" = "ok") {
  toaster.show((props) => (
    <Toast toastId={props.toastId} class={cx("pointer-events-auto flex w-80 items-start gap-2 rounded-xl p-3 shadow-lg ring-1", tone === "error" ? "bg-red-50 ring-red-200 dark:bg-red-950 dark:ring-red-900" : "bg-white ring-slate-200 dark:bg-slate-900 dark:ring-slate-700")}>
      <Show when={tone === "error"} fallback={<I.Check class="mt-0.5 size-4 text-emerald-600" />}>
        <I.AlertTriangle class="mt-0.5 size-4 text-red-600" />
      </Show>
      <div class="min-w-0 flex-1">
        <Toast.Title class="text-sm font-medium">{title}</Toast.Title>
        <Show when={body}>
          <Toast.Description class="break-words text-xs text-slate-600 dark:text-slate-400">{body}</Toast.Description>
        </Show>
      </div>
      <Toast.CloseButton class="text-slate-400 hover:text-slate-600" aria-label="close">
        <I.X class="size-4" />
      </Toast.CloseButton>
    </Toast>
  ));
}

export function Toasts() {
  return (
    <Portal>
      <Toast.Region duration={5000}>
        <Toast.List class="fixed right-3 bottom-20 z-[60] flex flex-col gap-2 md:bottom-3" />
      </Toast.Region>
    </Portal>
  );
}

/** Run an action, toast the error (or the success message). */
export async function act<T>(f: () => Promise<T>, ok?: string): Promise<T | undefined> {
  try {
    const r = await f();
    if (ok) notify(ok);
    return r;
  } catch (e: any) {
    notify("Failed", e?.message ?? String(e), "error");
    return undefined;
  }
}

export function CopyButton(p: { text: string }) {
  const [done, setDone] = createSignal(false);
  return (
    <Button
      size="sm"
      variant="ghost"
      onClick={async () => {
        await navigator.clipboard?.writeText(p.text);
        setDone(true);
        setTimeout(() => setDone(false), 1500);
      }}
      aria-label="copy"
    >
      <Show when={done()} fallback={<I.Copy class="size-3.5" />}>
        <I.Check class="size-3.5 text-emerald-600" />
      </Show>
      {done() ? "copied" : "copy"}
    </Button>
  );
}

/** Tiny SVG line chart. */
export function Sparkline(p: { values: number[]; class?: string; label?: string }) {
  const path = () => {
    const v = p.values;
    if (v.length < 2) return "";
    const max = Math.max(...v, 1e-9);
    return v.map((x, i) => `${i ? "L" : "M"}${((i / (v.length - 1)) * 100).toFixed(2)},${(30 - (x / max) * 28).toFixed(2)}`).join(" ");
  };
  return (
    <svg viewBox="0 0 100 30" preserveAspectRatio="none" class={cx("h-10 w-full", p.class)} role="img" aria-label={p.label}>
      <path d={path()} fill="none" stroke="currentColor" stroke-width="1.5" vector-effect="non-scaling-stroke" />
    </svg>
  );
}
