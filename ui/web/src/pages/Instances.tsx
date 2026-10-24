import { For, Show, createMemo, createSignal } from "solid-js";
import InstanceCard from "../components/InstanceCard";
import { Empty, PageHeader, inputCls } from "../components/ui";
import * as I from "../icons";
import { isLive, state } from "../store";
import type { Session } from "../types";

type Group = "node" | "ip" | "template" | "task" | "status" | "none";

export default function Instances() {
  const [q, setQ] = createSignal("");
  const [group, setGroup] = createSignal<Group>("node");
  const [showEnded, setShowEnded] = createSignal(false);
  const [status, setStatus] = createSignal("");
  const ipOf = (s: Session) => state.nodes.find((n) => n.name === s.node)?.observed_ip ?? "local";
  const list = createMemo(() =>
    state.sessions.filter((s) => {
      if (!showEnded() && !isLive(s) && !["dead", "halted", "error"].includes(s.state)) return false;
      if (status() && s.state !== status()) return false;
      const t = q().toLowerCase();
      return !t || [s.id, s.agent, s.node, s.title ?? "", s.current_task ?? "", s.task_id ? `#${s.task_id}` : ""].some((x) => x.toLowerCase().includes(t));
    }),
  );
  const groups = createMemo(() => {
    const key = (s: Session): string =>
      ({ node: s.node, ip: ipOf(s), template: s.agent, task: s.task_id ? `task #${s.task_id}` : s.run_id ? `run ${s.run_id}` : "no task", status: s.state, none: "" })[group()];
    const m = new Map<string, Session[]>();
    for (const s of list()) m.set(key(s), [...(m.get(key(s)) ?? []), s]);
    return [...m.entries()].sort((a, b) => a[0].localeCompare(b[0]));
  });
  return (
    <>
      <PageHeader title="Instances" subtitle={`${list().length} shown`} />
      <div class="mb-4 flex flex-wrap items-center gap-2">
        <div class="relative min-w-48 flex-1">
          <I.Search class="pointer-events-none absolute top-2.5 left-2.5 size-4 text-slate-400" />
          <input class={`${inputCls} pl-8`} placeholder="Filter by id, agent, node, task…" value={q()} onInput={(e) => setQ(e.currentTarget.value)} aria-label="filter" />
        </div>
        <select class={`${inputCls} w-auto`} value={group()} onChange={(e) => setGroup(e.currentTarget.value as Group)} aria-label="group by">
          <option value="node">Group: node</option>
          <option value="ip">Group: egress IP</option>
          <option value="template">Group: template</option>
          <option value="task">Group: task</option>
          <option value="status">Group: status</option>
          <option value="none">No grouping</option>
        </select>
        <select class={`${inputCls} w-auto`} value={status()} onChange={(e) => setStatus(e.currentTarget.value)} aria-label="status">
          <option value="">Any status</option>
          <For each={["running", "idle", "blocked", "parked", "starting", "error", "dead", "halted", "stopped", "lost", "unreachable"]}>{(s) => <option value={s}>{s}</option>}</For>
        </select>
        <label class="inline-flex items-center gap-2 text-sm">
          <input type="checkbox" checked={showEnded()} onChange={(e) => setShowEnded(e.currentTarget.checked)} /> include stopped
        </label>
      </div>
      <Show when={list().length} fallback={<Empty title="No instances match">Start one from Templates → New instance.</Empty>}>
        <div class="space-y-6">
          <For each={groups()}>
            {([g, items]) => (
              <section>
                <Show when={g}>
                  <h2 class="mb-2 text-sm font-semibold text-slate-600 dark:text-slate-400">
                    {g} <span class="font-normal text-slate-400">({items.length})</span>
                  </h2>
                </Show>
                <div class="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
                  <For each={items}>{(s) => <InstanceCard s={s} />}</For>
                </div>
              </section>
            )}
          </For>
        </div>
      </Show>
    </>
  );
}
