// The work in progress as graphs: one per task (or workflow run), showing who
// started whom, on which node and model, in what state, since when.
import { A, useLocation } from "@solidjs/router";
import { For, Show, createEffect, createMemo, createSignal } from "solid-js";
import { Card, Empty, NodeChip, PageHeader, Pill, StatusBadge, nodeColor } from "../components/ui";
import * as I from "../icons";
import { isLive, state } from "../store";
import type { Session } from "../types";
import { ago, billed, clock, cx, money } from "../util";

const W = 232; // card
const H = 132;
const GX = 44; // between generations
const GY = 14; // between siblings

interface Placed { s: Session; x: number; y: number }
interface Edge { from: Placed; to: Placed; replaced: boolean }
interface Group { key: string; title: string; href?: string; label?: string; status?: string; sessions: Session[] }

const away = (s: Session) => isLive(s) || !!s.preempted || !!s.task_paused || s.state === "halted";

/** Children to the right of their parent, each subtree on its own rows. */
export function layout(sessions: Session[]) {
  const here = new Map(sessions.map((s) => [s.id, s]));
  const kids = new Map<string, Session[]>();
  const roots: Session[] = [];
  for (const s of [...sessions].sort((a, b) => a.started_at - b.started_at)) {
    if (s.parent && here.has(s.parent) && s.parent !== s.id) kids.set(s.parent, [...(kids.get(s.parent) ?? []), s]);
    else roots.push(s);
  }
  const placed = new Map<string, Placed>();
  let row = 0;
  const place = (s: Session, depth: number): number => {
    const mine = kids.get(s.id) ?? [];
    let y: number;
    if (mine.length) {
      const ys = mine.map((k) => place(k, depth + 1));
      y = (ys[0] + ys[ys.length - 1]) / 2;
    } else {
      y = row++ * (H + GY);
    }
    placed.set(s.id, { s, x: depth * (W + GX), y });
    return y;
  };
  for (const r of roots) place(r, 0);
  const edges: Edge[] = [];
  for (const p of placed.values()) {
    if (p.s.parent && placed.has(p.s.parent)) edges.push({ from: placed.get(p.s.parent)!, to: p, replaced: false });
    if (p.s.replaces && placed.has(p.s.replaces)) edges.push({ from: placed.get(p.s.replaces)!, to: p, replaced: true });
  }
  const all = [...placed.values()];
  return { placed: all, edges, width: Math.max(0, ...all.map((p) => p.x)) + W, height: Math.max(0, ...all.map((p) => p.y)) + H };
}

function Node(p: { at: Placed }) {
  const s = () => p.at.s;
  const relay = () => state.nodes.find((n) => n.name === s().node)?.role === "relay";
  const questions = () => state.questions.filter((q) => q.asker === s().id).length;
  return (
    <A href={`/instances/${s().id}`} class="absolute block" style={{ left: `${p.at.x}px`, top: `${p.at.y}px`, width: `${W}px`, height: `${H}px` }} data-session={s().id}>
      <Card accent={nodeColor(s().node)} striped={s().state === "dead"} class={cx("flex h-full flex-col gap-1 p-2.5 text-xs hover:ring-slate-400 dark:hover:ring-slate-600", !away(s()) && "opacity-60")}>
        <div class="flex items-start justify-between gap-1">
          <div class="min-w-0">
            <div class="flex items-center gap-1 truncate text-sm font-medium">
              <Show when={s().role === "coordinator"}><I.Network class="size-3.5 shrink-0 text-indigo-500" aria-label="coordinator" /></Show>
              <span class="truncate" title={s().title || s().agent}>{s().title || s().agent}</span>
            </div>
            <div class="truncate font-mono text-[11px] text-slate-500" title={s().id}>{s().agent}</div>
          </div>
          <StatusBadge state={s().state} questions={questions()} />
        </div>
        <div class="flex min-w-0 items-center gap-1.5">
          <NodeChip name={s().node} relay={relay()} />
          <span class="truncate font-mono text-[11px] text-slate-600 dark:text-slate-300" title={`model ${s().model ?? "unknown"}`}>{s().model ?? "—"}</span>
        </div>
        <div class="min-h-0 flex-1 truncate text-slate-600 dark:text-slate-400" title={s().current_task || s().error || ""}>
          <Show when={s().preempted || s().task_paused} fallback={s().error || s().current_task || " "}>
            paused for {s().preempted?.by ?? "its task"}
          </Show>
        </div>
        <div class="flex items-center gap-2 text-[11px] text-slate-500">
          <span title={`started ${new Date(s().started_at).toLocaleString()}`}>{clock(s().started_at)} · {ago(s().started_at)}</span>
          <span title="cost">{money(billed(s()).cost_usd)}</span>
          <Show when={(s().held ?? 0) > 0}>
            <span class="ml-auto inline-flex items-center gap-0.5 font-medium text-amber-700 dark:text-amber-400" title="messages waiting until it runs again">
              <I.Mail class="size-3" />{s().held}
            </span>
          </Show>
        </div>
      </Card>
    </A>
  );
}

function Graph(p: { sessions: Session[] }) {
  const g = createMemo(() => layout(p.sessions));
  return (
    <div class="overflow-x-auto p-1">
      <div class="relative" style={{ width: `${g().width}px`, height: `${g().height}px` }}>
        <svg class="absolute inset-0 text-slate-300 dark:text-slate-700" width={g().width} height={g().height} aria-hidden="true">
          <For each={g().edges}>
            {(e) => {
              const x1 = e.from.x + W, y1 = e.from.y + H / 2, x2 = e.to.x, y2 = e.to.y + H / 2;
              // A replacement sits beside the session it replaced: join their top edges.
              const d = e.replaced ? `M${e.from.x + W / 2} ${e.from.y} C${e.from.x + W / 2} ${e.from.y - 12},${e.to.x + W / 2} ${e.to.y - 12},${e.to.x + W / 2} ${e.to.y}` : `M${x1} ${y1} C${x1 + GX / 2} ${y1},${x2 - GX / 2} ${y2},${x2} ${y2}`;
              return <path d={d} fill="none" stroke="currentColor" stroke-width="1.5" stroke-dasharray={e.replaced ? "4 3" : undefined} />;
            }}
          </For>
        </svg>
        <For each={g().placed}>{(at) => <Node at={at} />}</For>
      </div>
    </div>
  );
}

export default function Work() {
  const loc = useLocation();
  const [finished, setFinished] = createSignal(false);
  const groups = createMemo<Group[]>(() => {
    const m = new Map<string, Group>();
    for (const s of state.sessions) {
      const t = s.task_id != null ? state.tasks.find((x) => x.id === s.task_id) : undefined;
      const key = s.task_id != null ? `task-${s.task_id}` : s.run_id ? `run-${s.run_id}` : "hand";
      const g = m.get(key) ?? {
        key,
        title: s.task_id != null ? `#${s.task_id} ${t?.title ?? ""}` : s.run_id ? `Workflow ${state.runs.find((r) => r.id === s.run_id)?.workflow ?? s.run_id}` : "Started by hand",
        href: s.task_id != null ? "/tasks" : undefined,
        label: t?.label,
        status: t?.status,
        sessions: [],
      };
      g.sessions.push(s);
      m.set(key, g);
    }
    return [...m.values()]
      .map((g) => (g.key === "hand" && !finished() ? { ...g, sessions: g.sessions.filter(away) } : g))
      .filter((g) => g.sessions.length && (finished() || g.sessions.some(away)))
      .sort((a, b) => Math.max(...b.sessions.map((s) => s.started_at)) - Math.max(...a.sessions.map((s) => s.started_at)));
  });
  const nodes = createMemo(() => [...new Set(groups().flatMap((g) => g.sessions.map((s) => s.node)))].sort());
  createEffect(() => {
    const id = loc.hash.slice(1);
    if (id && groups().length) setTimeout(() => document.getElementById(id)?.scrollIntoView({ block: "start", behavior: "smooth" }), 50);
  });
  return (
    <>
      <PageHeader
        title="Work map"
        subtitle={`${groups().length} group${groups().length === 1 ? "" : "s"} of work · ${groups().reduce((n, g) => n + g.sessions.filter(isLive).length, 0)} live`}
        actions={
          <label class="inline-flex items-center gap-2 text-sm">
            <input type="checkbox" checked={finished()} onChange={(e) => setFinished(e.currentTarget.checked)} /> Show finished
          </label>
        }
      />
      <Show when={nodes().length}>
        <div class="mb-4 flex flex-wrap items-center gap-2 text-xs text-slate-500" aria-label="legend">
          <span>Left edge and chip: node</span>
          <For each={nodes()}>{(n) => <NodeChip name={n} relay={state.nodes.find((x) => x.name === n)?.role === "relay"} />}</For>
          <span class="ml-2 inline-flex items-center gap-1"><I.Network class="size-3.5 text-indigo-500" /> coordinator</span>
          <span class="inline-flex items-center gap-1"><span class="inline-block w-5 border-t-2 border-slate-400" /> started</span>
          <span class="inline-flex items-center gap-1"><span class="inline-block w-5 border-t-2 border-dashed border-slate-400" /> replaced</span>
        </div>
      </Show>
      <div class="space-y-4">
        <For each={groups()} fallback={<Empty icon={I.Network} title="Nothing is running">Start a task or an instance and it appears here.</Empty>}>
          {(g) => (
            <section id={g.key} class="scroll-mt-16">
              <Card class="p-3">
                <div class="mb-3 flex flex-wrap items-center gap-2">
                  <Show when={g.href} fallback={<h2 class="font-semibold break-words">{g.title}</h2>}>
                    <A href={g.href!} class="font-semibold break-words hover:underline"><h2>{g.title}</h2></A>
                  </Show>
                  <Show when={g.label}><Pill tone={g.label === "right_now" ? "red" : g.label === "urgent" ? "orange" : "slate"}>{g.label!.replace("_", " ")}</Pill></Show>
                  <Show when={g.status}><Pill tone={g.status === "running" ? "emerald" : "slate"}>{g.status}</Pill></Show>
                  <span class="ml-auto text-xs text-slate-500">
                    {g.sessions.filter(isLive).length}/{g.sessions.length} live · started {ago(Math.min(...g.sessions.map((s) => s.started_at)))} ago · {money(g.sessions.reduce((n, s) => n + billed(s).cost_usd, 0))}
                  </span>
                </div>
                <Graph sessions={g.sessions} />
              </Card>
            </section>
          )}
        </For>
      </div>
    </>
  );
}
