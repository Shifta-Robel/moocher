import { A, useLocation } from "@solidjs/router";
import { For, JSX, Show, createSignal, onMount } from "solid-js";
import { Dynamic } from "solid-js/web";
import * as I from "../icons";
import { auth } from "../api";
import { state } from "../store";
import { cx } from "../util";
import { Toasts } from "./ui";

type Theme = "system" | "light" | "dark";
const readTheme = (): Theme => {
  try {
    return (localStorage.getItem("moo-theme") as Theme) || "system";
  } catch {
    return "system";
  }
};
export const [theme, setThemeSig] = createSignal<Theme>(readTheme());
export function setTheme(t: Theme) {
  setThemeSig(t);
  try {
    localStorage.setItem("moo-theme", t);
  } catch {}
  applyTheme();
}
function applyTheme() {
  const dark = theme() === "dark" || (theme() === "system" && matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
}

const NAV = [
  { href: "/", label: "Overview", icon: I.LayoutDashboard, mobile: true },
  { href: "/instances", label: "Instances", icon: I.Boxes, mobile: true },
  { href: "/questions", label: "Questions", icon: I.CircleHelp, mobile: true, badge: () => state.questions.length },
  { href: "/tasks", label: "Tasks", icon: I.ListChecks, mobile: true, badge: () => state.halts.length },
  { href: "/quotas", label: "Quotas", icon: I.Gauge },
  { href: "/templates", label: "Templates", icon: I.Layers },
  { href: "/nodes", label: "Nodes & proxies", icon: I.Server },
  { href: "/artifacts", label: "Artifacts", icon: I.Archive },
  { href: "/notifications", label: "Notifications", icon: I.Bell, badge: () => state.unread },
  { href: "/audit", label: "Audit", icon: I.ScrollText },
  { href: "/settings", label: "Settings", icon: I.Settings },
];

function NavLink(p: { href: string; label: string; icon: any; badge?: () => number; compact?: boolean }) {
  const loc = useLocation();
  const active = () => (p.href === "/" ? loc.pathname === "/" : loc.pathname.startsWith(p.href));
  return (
    <A
      href={p.href}
      class={cx(
        "relative flex items-center gap-3 rounded-lg text-sm font-medium transition-colors",
        p.compact ? "flex-col gap-0.5 px-2 py-1.5 text-[11px]" : "px-3 py-2",
        active() ? "bg-indigo-600/10 text-indigo-700 dark:bg-indigo-400/10 dark:text-indigo-300" : "text-slate-600 hover:bg-slate-200/60 dark:text-slate-400 dark:hover:bg-slate-800",
      )}
      aria-current={active() ? "page" : undefined}
    >
      <Dynamic component={p.icon} class="size-5 shrink-0" />
      <span>{p.label}</span>
      <Show when={(p.badge?.() ?? 0) > 0}>
        <span class={cx("rounded-full bg-amber-500 px-1.5 text-[10px] font-bold text-white", p.compact ? "absolute -top-0.5 right-1" : "ml-auto")}>{p.badge!()}</span>
      </Show>
    </A>
  );
}

function StatusStrip() {
  const online = () => state.nodes.filter((n) => n.connected).length;
  const exhausted = () => new Set(state.quotas.filter((q) => q.exhausted).map((q) => q.provider)).size;
  return (
    <div class="flex flex-wrap items-center gap-2 text-xs">
      <span class={cx("inline-flex items-center gap-1 rounded-full px-2 py-0.5 ring-1", online() === state.nodes.length ? "ring-emerald-300 text-emerald-700 dark:text-emerald-300 dark:ring-emerald-800" : "ring-amber-300 text-amber-800 dark:text-amber-300 dark:ring-amber-800")} title="nodes connected">
        <I.Server class="size-3.5" /> {online()}/{state.nodes.length} nodes
      </span>
      <Show when={exhausted() > 0}>
        <A href="/quotas" class="inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-sky-800 ring-1 ring-sky-300 dark:text-sky-300 dark:ring-sky-800">
          <I.Clock class="size-3.5" /> {exhausted()} provider{exhausted() > 1 ? "s" : ""} waiting for a reset
        </A>
      </Show>
      <Show when={state.questions.length > 0}>
        <A href="/questions" class="inline-flex items-center gap-1 rounded-full bg-amber-100 px-2 py-0.5 text-amber-900 dark:bg-amber-950 dark:text-amber-300">
          <I.CircleHelp class="size-3.5" /> {state.questions.length} question{state.questions.length > 1 ? "s" : ""}
        </A>
      </Show>
      <Show when={state.halts.length > 0}>
        <A href="/tasks" class="inline-flex items-center gap-1 rounded-full bg-orange-100 px-2 py-0.5 text-orange-900 dark:bg-orange-950 dark:text-orange-300">
          <I.PauseCircle class="size-3.5" /> {state.halts.length} halted
        </A>
      </Show>
      <Show when={!state.connected}>
        <span class="inline-flex items-center gap-1 rounded-full bg-zinc-200 px-2 py-0.5 text-zinc-800 dark:bg-zinc-800 dark:text-zinc-300" title="live updates reconnecting">
          <I.WifiOff class="size-3.5" /> offline
        </span>
      </Show>
    </div>
  );
}

export function ThemeButton() {
  const next = () => (theme() === "system" ? "light" : theme() === "light" ? "dark" : "system");
  return (
    <button class="rounded-lg p-2 text-slate-600 hover:bg-slate-200/70 dark:text-slate-400 dark:hover:bg-slate-800" onClick={() => setTheme(next())} aria-label={`theme: ${theme()} (switch to ${next()})`} title={`theme: ${theme()}`}>
      <Dynamic component={theme() === "dark" ? I.Moon : theme() === "light" ? I.Sun : I.Monitor} class="size-5" />
    </button>
  );
}

export default function Layout(p: { children?: JSX.Element }) {
  const [more, setMore] = createSignal(false);
  onMount(() => {
    applyTheme();
    matchMedia("(prefers-color-scheme: dark)").addEventListener("change", applyTheme);
  });
  const logout = async () => {
    await auth("logout", {}).catch(() => {});
    location.assign("/login");
  };
  return (
    <div class="min-h-dvh md:flex">
      <aside class="sticky top-0 hidden h-dvh w-60 shrink-0 flex-col border-r border-slate-200 bg-white/70 p-3 md:flex dark:border-slate-800 dark:bg-slate-900/60">
        <div class="mb-4 flex items-center gap-2 px-2 pt-1">
          <img src="/icon.svg" class="size-7" alt="" />
          <span class="font-semibold">moocher</span>
        </div>
        <nav class="flex flex-1 flex-col gap-0.5" aria-label="main">
          <For each={NAV}>{(n) => <NavLink {...n} />}</For>
        </nav>
        <div class="flex items-center justify-between border-t border-slate-200 pt-2 dark:border-slate-800">
          <span class="truncate px-2 text-xs text-slate-500" title="logged in">{state.info?.user}</span>
          <div class="flex">
            <ThemeButton />
            <button class="rounded-lg p-2 text-slate-600 hover:bg-slate-200/70 dark:text-slate-400 dark:hover:bg-slate-800" onClick={logout} aria-label="log out" title="log out">
              <I.LogOut class="size-5" />
            </button>
          </div>
        </div>
      </aside>
      <div class="min-w-0 flex-1">
        <header class="sticky top-0 z-30 flex items-center justify-between gap-2 border-b border-slate-200 bg-slate-50/85 px-4 py-2 backdrop-blur md:px-6 dark:border-slate-800 dark:bg-slate-950/85">
          <div class="flex items-center gap-2 md:hidden">
            <img src="/icon.svg" class="size-6" alt="" />
          </div>
          <StatusStrip />
          <div class="md:hidden">
            <ThemeButton />
          </div>
        </header>
        <main class="mx-auto max-w-7xl px-4 py-5 pb-24 md:px-6 md:pb-8">{p.children}</main>
      </div>
      <nav class="fixed inset-x-0 bottom-0 z-30 grid grid-cols-5 border-t border-slate-200 bg-white/95 px-1 pt-1 pb-[max(env(safe-area-inset-bottom),0.25rem)] backdrop-blur md:hidden dark:border-slate-800 dark:bg-slate-900/95" aria-label="main">
        <For each={NAV.filter((n) => n.mobile)}>{(n) => <NavLink {...n} compact />}</For>
        <button class="flex flex-col items-center gap-0.5 rounded-lg px-2 py-1.5 text-[11px] font-medium text-slate-600 dark:text-slate-400" onClick={() => setMore(!more())} aria-expanded={more()}>
          <I.Menu class="size-5" />
          More
          <Show when={state.unread > 0}>
            <span class="absolute top-1 right-3 size-2 rounded-full bg-amber-500" />
          </Show>
        </button>
      </nav>
      <Show when={more()}>
        <div class="fixed inset-0 z-40 bg-slate-950/40 md:hidden" onClick={() => setMore(false)}>
          <div class="absolute inset-x-2 bottom-20 rounded-2xl bg-white p-2 shadow-xl dark:bg-slate-900" onClick={(e) => e.stopPropagation()}>
            <div class="grid grid-cols-2 gap-1" onClick={() => setMore(false)}>
              <For each={NAV.filter((n) => !n.mobile)}>{(n) => <NavLink {...n} />}</For>
              <button class="flex items-center gap-3 rounded-lg px-3 py-2 text-sm font-medium text-slate-600 dark:text-slate-400" onClick={logout}>
                <I.LogOut class="size-5" /> Log out
              </button>
            </div>
          </div>
        </div>
      </Show>
      <Toasts />
    </div>
  );
}
