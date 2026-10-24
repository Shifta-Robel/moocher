import { A } from "@solidjs/router";
import { For, Show } from "solid-js";
import InstanceCard from "../components/InstanceCard";
import { Card, Empty, Importance, NodeChip, PageHeader, nodeColor } from "../components/ui";
import * as I from "../icons";
import { isLive, state } from "../store";
import { ago, money, num } from "../util";

export default function Overview() {
  const live = () => state.sessions.filter(isLive);
  const attention = () => state.sessions.filter((s) => ["dead", "error", "halted", "lost", "unreachable"].includes(s.state) && Date.now() - s.updated_at < 86_400_000);
  const byNode = () => {
    const m = new Map<string, typeof state.sessions>();
    for (const n of state.nodes.filter((n) => n.role !== "relay")) m.set(n.name, []);
    for (const s of live()) m.set(s.node, [...(m.get(s.node) ?? []), s]);
    return [...m.entries()];
  };
  return (
    <>
      <PageHeader
        title="Overview"
        subtitle={`${live().length} live instance${live().length === 1 ? "" : "s"} · ${num(state.totals.input + state.totals.output)} tokens · ${money(state.totals.cost_usd)} spent`}
        actions={
          <A href="/templates" class="inline-flex items-center gap-1.5 rounded-lg bg-indigo-600 px-3.5 py-2 text-sm font-medium text-white hover:bg-indigo-500">
            <I.Plus class="size-4" /> New instance
          </A>
        }
      />
      <Show when={state.questions.length || state.halts.length}>
        <div class="mb-6 grid gap-3 md:grid-cols-2">
          <For each={state.questions.slice(0, 3)}>
            {(q) => (
              <A href="/questions">
                <Card class="flex items-start gap-3 p-3">
                  <I.CircleHelp class="mt-0.5 size-5 shrink-0 text-amber-500" />
                  <div class="min-w-0 flex-1">
                    <div class="mb-1 flex items-center gap-2">
                      <Importance level={q.importance} />
                      <span class="truncate text-xs text-slate-500">
                        {q.asker_agent} · blocks {q.blocked} · {ago(q.created_at)}
                      </span>
                    </div>
                    <p class="line-clamp-2 text-sm">{q.text}</p>
                  </div>
                </Card>
              </A>
            )}
          </For>
          <For each={state.halts}>
            {(h) => (
              <A href="/tasks">
                <Card class="flex items-start gap-3 p-3 ring-orange-300 dark:ring-orange-800">
                  <I.PauseCircle class="mt-0.5 size-5 shrink-0 text-orange-500" />
                  <div class="text-sm">
                    <div class="font-medium">{h.scope} halted</div>
                    <div class="text-slate-500">
                      {h.dead_session} died ({h.reason}) · spent {money(h.spent_usd)}
                    </div>
                  </div>
                </Card>
              </A>
            )}
          </For>
        </div>
      </Show>
      <Show when={attention().length}>
        <h2 class="mb-2 text-sm font-semibold text-red-700 dark:text-red-400">Needs attention</h2>
        <div class="mb-6 grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
          <For each={attention()}>{(s) => <InstanceCard s={s} />}</For>
        </div>
      </Show>
      <Show when={state.nodes.length} fallback={<Empty icon={I.Server} title="No nodes yet">Start opencode-node on this machine, or add one with Nodes → Add machine.</Empty>}>
        <div class="space-y-6">
          <For each={byNode()}>
            {([node, sessions]) => {
              const n = () => state.nodes.find((x) => x.name === node);
              return (
                <section>
                  <div class="mb-2 flex items-center gap-2">
                    <span class="size-2.5 rounded-full" style={{ "background-color": nodeColor(node) }} />
                    <NodeChip name={node} />
                    <span class="text-xs text-slate-500">
                      {n()?.connected ? `${sessions.length}/${n()?.max_sessions} slots · ${n()?.link_rtt_ms != null ? Math.round(n()!.link_rtt_ms!) + " ms" : ""}` : "offline"}
                    </span>
                  </div>
                  <Show when={sessions.length} fallback={<p class="text-sm text-slate-500">Nothing running.</p>}>
                    <div class="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
                      <For each={sessions}>{(s) => <InstanceCard s={s} />}</For>
                    </div>
                  </Show>
                </section>
              );
            }}
          </For>
        </div>
      </Show>
    </>
  );
}
