import { A } from "@solidjs/router";
import { Show } from "solid-js";
import * as I from "../icons";
import { state } from "../store";
import type { Session } from "../types";
import { ago, billed, money, num, todoProgress } from "../util";
import { Card, NodeChip, StatusBadge, nodeColor } from "./ui";

export default function InstanceCard(p: { s: Session }) {
  const questions = () => state.questions.filter((q) => q.asker === p.s.id).length;
  const relay = () => state.nodes.find((n) => n.name === p.s.node)?.role === "relay";
  return (
    <A href={`/instances/${p.s.id}`} class="block">
      <Card accent={nodeColor(p.s.node)} striped={p.s.state === "dead"} class="h-full p-3 hover:ring-slate-300 dark:hover:ring-slate-700">
        <div class="flex items-start justify-between gap-2">
          <div class="min-w-0">
            <div class="truncate font-medium">{p.s.title || p.s.agent}</div>
            <div class="truncate font-mono text-xs text-slate-500">
              {p.s.agent} · {p.s.id}
            </div>
          </div>
          <StatusBadge state={p.s.state} questions={questions()} />
        </div>
        <div class="mt-2 line-clamp-2 min-h-[2.5em] text-sm text-slate-600 dark:text-slate-400">
          <Show when={p.s.error} fallback={p.s.current_task || p.s.last_text || "—"}>
            <span class="text-red-700 dark:text-red-400">{p.s.error}</span>
          </Show>
        </div>
        <div class="mt-2 flex flex-wrap items-center gap-2 text-xs text-slate-500">
          <NodeChip name={p.s.node} relay={relay()} />
          <span class="inline-flex items-center gap-1" title="todos completed">
            <I.ListChecks class="size-3.5" />
            {todoProgress(p.s.todos)}
          </span>
          <span title="tokens">{num(billed(p.s).total)} tok</span>
          <span title="cost">{money(billed(p.s).cost_usd)}</span>
          <Show when={(p.s.held ?? 0) > 0}>
            <span class="inline-flex items-center gap-1 text-amber-700 dark:text-amber-400" title="messages waiting until it runs again">
              <I.Mail class="size-3.5" />{p.s.held}
            </span>
          </Show>
          <span class="ml-auto" title="age">{ago(p.s.started_at)}</span>
          <Show when={p.s.attached_by && (p.s.attached_until ?? 0) > Date.now()}>
            <span class="inline-flex items-center gap-1 text-indigo-600 dark:text-indigo-400" title={`attached by ${p.s.attached_by}`}>
              <I.ExternalLink class="size-3.5" /> attached
            </span>
          </Show>
        </div>
      </Card>
    </A>
  );
}
