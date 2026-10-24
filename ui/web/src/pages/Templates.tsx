import { useNavigate } from "@solidjs/router";
import { For, Show, createMemo, createResource, createSignal } from "solid-js";
import { get, post } from "../api";
import { Button, Card, Empty, Field, Modal, NodeChip, PageHeader, Pill, act, inputCls } from "../components/ui";
import * as I from "../icons";
import { state } from "../store";
import type { AgentInfo } from "../types";

export default function Templates() {
  const nav = useNavigate();
  const [lib] = createResource(() => get<any>("/templates").catch(() => ({ templates: [] })));
  const agents = createMemo(() => {
    const m = new Map<string, { a: AgentInfo; nodes: string[] }>();
    for (const n of state.nodes.filter((n) => n.connected))
      for (const a of n.agents) {
        const e = m.get(a.name) ?? { a, nodes: [] };
        if (a.secret_ok && !a.errors.length) e.nodes.push(n.name);
        m.set(a.name, e);
      }
    return [...m.values()].sort((x, y) => x.a.name.localeCompare(y.a.name));
  });
  const [pick, setPick] = createSignal<AgentInfo | null>(null);
  const [prompt, setPrompt] = createSignal("");
  const [node, setNode] = createSignal("");
  const [append, setAppend] = createSignal("");
  const [budget, setBudget] = createSignal("");
  const [route, setRoute] = createSignal("");
  const [shared, setShared] = createSignal("");
  const [coord, setCoord] = createSignal(false);
  const live = (name: string) => state.sessions.filter((s) => s.agent === name && ["starting", "idle", "running", "blocked", "parked"].includes(s.state)).length;
  const spawn = async () => {
    const a = pick()!;
    const overrides: any = {};
    if (append()) overrides.role_append = append();
    if (budget()) overrides.budget_usd = Number(budget());
    if (route()) overrides.route = route();
    const r = await act(
      () =>
        post<any>("/sessions", {
          agent: a.name,
          prompt: prompt() || undefined,
          node: node() || undefined,
          workspace: shared() ? { mode: "shared", name: shared() } : undefined,
          overrides,
          role: coord() ? "coordinator" : "worker",
        }),
      "Instance started",
    );
    if (r) nav(`/instances/${r.id}`);
  };
  return (
    <>
      <PageHeader title="Templates" subtitle="Agent profiles on your nodes and the hub's template library." />
      <Show when={agents().length} fallback={<Empty icon={I.Layers} title="No agents configured">Add agents/*.yaml on a node or templates/*.yaml on the hub.</Empty>}>
        <div class="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
          <For each={agents()}>
            {({ a, nodes }) => {
              const t = () => lib()?.templates?.find((x: any) => x.name === a.name);
              return (
                <Card class={`flex flex-col p-4 ${a.tier === "best_effort" ? "border border-dashed border-slate-400 dark:border-slate-600" : ""}`}>
                  <div class="mb-1 flex items-start justify-between gap-2">
                    <div class="font-semibold">{a.name}</div>
                    <Pill tone={a.source === "hub" ? "indigo" : "slate"}>{a.source}</Pill>
                  </div>
                  <p class="mb-2 text-sm text-slate-600 dark:text-slate-400">{a.description}</p>
                  <div class="mb-2 flex flex-wrap gap-1 text-xs">
                    <Pill>{a.provider}/{a.model}</Pill>
                    <Show when={a.tier === "best_effort"}>
                      <Pill tone="amber" title="free or keyless: best effort">best effort</Pill>
                    </Show>
                    <Pill>
                      live {live(a.name)}/{a.max_instances ?? "∞"}
                    </Pill>
                  </div>
                  <div class="mb-3 flex flex-wrap gap-1">
                    <For each={nodes} fallback={<span class="text-xs text-red-600">not runnable on any connected node</span>}>{(n) => <NodeChip name={n} />}</For>
                  </div>
                  <For each={t()?.not_runnable ?? []}>{(b: any) => <p class="text-xs text-slate-500">not on {b.node}: {b.problems.join("; ")}</p>}</For>
                  <div class="mt-auto">
                    <Button variant="primary" disabled={!nodes.length} onClick={() => { setPick(a); setPrompt(""); setNode(""); setAppend(""); setBudget(""); setRoute(""); setShared(""); setCoord(false); }}>
                      <I.Plus class="size-4" /> New instance
                    </Button>
                  </div>
                </Card>
              );
            }}
          </For>
        </div>
      </Show>
      <Modal open={!!pick()} onClose={() => setPick(null)} title={`New ${pick()?.name ?? ""} instance`} wide>
        <form class="space-y-3" onSubmit={(e) => { e.preventDefault(); spawn(); }}>
          <Field label="First prompt">
            <textarea class={inputCls} rows={5} value={prompt()} onInput={(e) => setPrompt(e.currentTarget.value)} placeholder="What should it do?" />
          </Field>
          <div class="grid gap-3 sm:grid-cols-2">
            <Field label="Node" hint="empty = best placement">
              <select class={inputCls} value={node()} onChange={(e) => setNode(e.currentTarget.value)}>
                <option value="">automatic</option>
                <For each={state.nodes.filter((n) => n.connected && n.agents.some((x) => x.name === pick()?.name))}>{(n) => <option value={n.name}>{n.name}</option>}</For>
              </select>
            </Field>
            <Field label="Shared workspace" hint="name; empty = its own">
              <input class={inputCls} value={shared()} onInput={(e) => setShared(e.currentTarget.value)} pattern="[a-z0-9-]*" />
            </Field>
          </div>
          <Show when={pick()?.overridable.role_append}>
            <Field label="Extra instructions for this instance">
              <textarea class={inputCls} rows={2} value={append()} onInput={(e) => setAppend(e.currentTarget.value)} />
            </Field>
          </Show>
          <div class="grid gap-3 sm:grid-cols-2">
            <Show when={pick()?.overridable.budget_usd}>
              <Field label={`Budget (USD, up to ${pick()?.overridable.budget_usd})`}>
                <input class={inputCls} type="number" step="0.01" min="0" max={pick()?.overridable.budget_usd ?? undefined} value={budget()} onInput={(e) => setBudget(e.currentTarget.value)} />
              </Field>
            </Show>
            <Show when={pick()?.overridable.routes.length}>
              <Field label="Preferred route">
                <select class={inputCls} value={route()} onChange={(e) => setRoute(e.currentTarget.value)}>
                  <option value="">template default</option>
                  <For each={pick()?.overridable.routes}>{(r) => <option value={r}>{r}</option>}</For>
                </select>
              </Field>
            </Show>
          </div>
          <label class="inline-flex items-center gap-2 text-sm">
            <input type="checkbox" checked={coord()} onChange={(e) => setCoord(e.currentTarget.checked)} /> coordinator (may start and stop other agents)
          </label>
          <div class="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => setPick(null)}>Cancel</Button>
            <Button type="submit" variant="primary">Start</Button>
          </div>
        </form>
      </Modal>
    </>
  );
}
