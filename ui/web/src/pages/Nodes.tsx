import { For, Show, createResource, createSignal } from "solid-js";
import { del, get, post } from "../api";
import { Button, Card, CopyButton, Empty, Field, Modal, NodeChip, PageHeader, Pill, act, inputCls } from "../components/ui";
import * as I from "../icons";
import { state, statsTick } from "../store";
import type { NodeInfo } from "../types";
import { ago, bytes } from "../util";

function Bar(p: { pct: number; label: string }) {
  return (
    <div class="flex items-center gap-2 text-xs">
      <span class="w-12 text-slate-500">{p.label}</span>
      <div class="h-1.5 flex-1 overflow-hidden rounded-full bg-slate-200 dark:bg-slate-800">
        <div class={`h-full ${p.pct >= 90 ? "bg-red-500" : p.pct >= 70 ? "bg-amber-500" : "bg-emerald-500"}`} style={{ width: `${Math.min(100, p.pct)}%` }} />
      </div>
      <span class="w-10 text-right tabular-nums">{Math.round(p.pct)}%</span>
    </div>
  );
}

export default function Nodes() {
  const [stats, { refetch }] = createResource(statsTick, () => get<any>("/stats").catch(() => ({ hosts: [] })));
  void refetch;
  const [allow, { refetch: refetchAllow }] = createResource(() => get<any>("/allowed-sources").catch(() => null));
  const [inviting, setInviting] = createSignal(false);
  const [name, setName] = createSignal("");
  const [relay, setRelay] = createSignal(false);
  const [join, setJoin] = createSignal<any>(null);
  const [cidr, setCidr] = createSignal("");
  const [note, setNote] = createSignal("");
  const [routeEdit, setRouteEdit] = createSignal<{ node: string; agent: string; routes: string[] } | null>(null);
  const [prefer, setPrefer] = createSignal("");
  const [failover, setFailover] = createSignal("");
  const host = (n: string) => stats()?.hosts?.find((h: any) => h.node === n);
  const invite = async () => {
    const r = await act(() => post<any>("/nodes/invite", { name: name(), role: relay() ? "relay" : "worker" }));
    if (r) setJoin(r);
  };
  const saveRoute = async (clear = false) => {
    const e = routeEdit()!;
    const route = clear ? null : { prefer: prefer(), failover: failover().split(",").map((s) => s.trim()).filter(Boolean) };
    if (await act(() => post(`/nodes/${e.node}/routes`, { agent: e.agent, route }), clear ? "Override removed" : "Route changed live")) setRouteEdit(null);
  };
  const lifecycle = (n: NodeInfo) => (
    <div class="flex flex-wrap gap-1">
      <Show when={n.enrollment === "pending"}>
        <Button size="sm" variant="primary" onClick={() => act(() => post(`/nodes/${n.name}/approve`), `${n.name} approved`)}>
          Approve
        </Button>
      </Show>
      <Show when={n.enrollment === "approved" || n.enrollment === "pending"}>
        <Button size="sm" variant="ghost" onClick={() => confirm(`Revoke ${n.name}? It is disconnected at once.`) && act(() => post(`/nodes/${n.name}/revoke`), `${n.name} revoked`)}>
          Revoke
        </Button>
      </Show>
      <Show when={!n.connected}>
        <Button size="sm" variant="ghost" onClick={() => confirm(`Forget ${n.name}?`) && act(() => del(`/nodes/${n.name}`), "Forgotten")}>
          Forget
        </Button>
      </Show>
      <Button size="sm" variant="ghost" onClick={() => act(() => post(`/nodes/${n.name}/probe`), "Egress IPs re-learned")} disabled={!n.connected}>
        Probe IPs
      </Button>
    </div>
  );
  return (
    <>
      <PageHeader
        title="Nodes & proxies"
        subtitle="Machines in the cluster. Relays only lend their IP to other nodes' LLM calls."
        actions={
          <Button variant="primary" onClick={() => { setName(""); setRelay(false); setJoin(null); setInviting(true); }}>
            <I.Plus class="size-4" /> Add machine
          </Button>
        }
      />
      <Show when={state.nodes.length} fallback={<Empty icon={I.Server} title="No nodes yet" />}>
        <div class="grid gap-3 lg:grid-cols-2">
          <For each={state.nodes}>
            {(n) => (
              <Card class="p-4">
                <div class="mb-2 flex flex-wrap items-center gap-2">
                  <NodeChip name={n.name} relay={n.role === "relay"} />
                  <Pill tone={n.connected ? "emerald" : "red"}>
                    <Show when={n.connected} fallback={<><I.WifiOff class="size-3" /> offline {ago(n.last_seen)}</>}>
                      <I.Check class="size-3" /> connected
                    </Show>
                  </Pill>
                  <Pill tone={n.enrollment === "pending" ? "amber" : n.enrollment === "revoked" ? "red" : "slate"}>{n.enrollment}</Pill>
                  <Pill>{n.role}</Pill>
                  <Show when={n.link_rtt_ms != null}>
                    <Pill title="hub ↔ node round trip">{Math.round(n.link_rtt_ms!)} ms</Pill>
                  </Show>
                  <Show when={n.observed_ip}>
                    <Pill>{n.observed_ip}</Pill>
                  </Show>
                </div>
                <Show when={host(n.name)}>
                  {(h) => (
                    <div class="mb-3 space-y-1">
                      <Bar label="load" pct={(h().load1 / Math.max(1, h().cpus)) * 100} />
                      <Bar label="memory" pct={100 - (h().mem_available / Math.max(1, h().mem_total)) * 100} />
                      <Bar label="disk" pct={100 - (h().disk_free / Math.max(1, h().disk_total)) * 100} />
                      <p class="text-xs text-slate-500">
                        {h().cpus} CPUs · {bytes(h().mem_available)} free of {bytes(h().mem_total)} · {bytes(h().disk_free)} disk free
                      </p>
                    </div>
                  )}
                </Show>
                <Show when={n.role !== "relay"}>
                  <p class="mb-1 text-xs text-slate-500">
                    {n.running}/{n.max_sessions} sessions · v{n.version}
                  </p>
                  <div class="mb-3 flex flex-wrap gap-1">
                    <For each={n.agents}>
                      {(a) => (
                        <button
                          class="rounded-md bg-slate-100 px-1.5 py-0.5 text-xs hover:bg-slate-200 dark:bg-slate-800 dark:hover:bg-slate-700"
                          title={`routes: ${a.routes.join(" → ")} (click to change live)`}
                          onClick={() => { setPrefer(a.routes[0] ?? "direct"); setFailover(a.routes.slice(1).join(", ")); setRouteEdit({ node: n.name, agent: a.name, routes: a.routes }); }}
                        >
                          {a.name}
                          <span class="text-slate-500"> · {a.routes.join("→")}</span>
                        </button>
                      )}
                    </For>
                  </div>
                </Show>
                {lifecycle(n)}
              </Card>
            )}
          </For>
        </div>
      </Show>
      <h2 class="mt-8 mb-2 font-semibold">Allowed source IPs</h2>
      <Card class="p-4">
        <p class="mb-3 text-sm text-slate-500">Enforced for: <b>{allow()?.enforce_for ?? "?"}</b>. Removing an entry disconnects the nodes it no longer covers.</p>
        <ul class="mb-3 divide-y divide-slate-100 text-sm dark:divide-slate-800">
          <For each={allow()?.entries ?? []} fallback={<li class="py-2 text-slate-500">empty</li>}>
            {(e: any) => (
              <li class="flex items-center gap-2 py-2">
                <span class="font-mono">{e.cidr}</span>
                <span class="text-slate-500">{e.note}</span>
                <span class="ml-auto text-xs text-slate-500">{e.added_by}</span>
                <Button size="sm" variant="ghost" aria-label={`remove ${e.cidr}`} onClick={async () => { await act(() => post("/allowed-sources/remove", { source: e.cidr }), "Removed"); refetchAllow(); }}>
                  <I.Trash class="size-3.5" />
                </Button>
              </li>
            )}
          </For>
        </ul>
        <form class="flex flex-col gap-2 sm:flex-row" onSubmit={async (e) => { e.preventDefault(); if (await act(() => post("/allowed-sources", { source: cidr(), note: note() }), "Allowed")) { setCidr(""); setNote(""); refetchAllow(); } }}>
          <input class={inputCls} placeholder="IP or CIDR, e.g. 203.0.113.7 or 10.0.0.0/8" value={cidr()} onInput={(e) => setCidr(e.currentTarget.value)} required />
          <input class={inputCls} placeholder="note" value={note()} onInput={(e) => setNote(e.currentTarget.value)} />
          <Button type="submit">Add</Button>
        </form>
      </Card>
      <Modal open={inviting()} onClose={() => setInviting(false)} title="Add a machine">
        <Show
          when={join()}
          fallback={
            <form class="space-y-3" onSubmit={(e) => { e.preventDefault(); invite(); }}>
              <Field label="Name" hint="[a-z0-9-], unique in the cluster">
                <input class={inputCls} value={name()} onInput={(e) => setName(e.currentTarget.value)} pattern="[a-z0-9\-]+" required />
              </Field>
              <label class="inline-flex items-center gap-2 text-sm">
                <input type="checkbox" checked={relay()} onChange={(e) => setRelay(e.currentTarget.checked)} /> relay only (lends its IP, runs no agents; needs an allowed source IP)
              </label>
              <div class="flex justify-end">
                <Button type="submit" variant="primary">Create join command</Button>
              </div>
            </form>
          }
        >
          {/* Focus moves to Copy, the next step (keeps keyboard use, Escape included, inside the dialog). */}
          <div class="space-y-3 text-sm" ref={(el) => queueMicrotask(() => el.querySelector<HTMLButtonElement>("button")?.focus())}>
            <p>Run this on the new machine (single use, valid for 15 minutes):</p>
            <pre class="overflow-x-auto rounded-lg bg-slate-950 p-3 text-xs text-slate-100">{join().command}</pre>
            <CopyButton text={join().command} />
            <Show when={join().approval_required}>
              <p class="text-slate-500">After it enrolls, approve it here (it shows as pending).</p>
            </Show>
          </div>
        </Show>
      </Modal>
      <Modal open={!!routeEdit()} onClose={() => setRouteEdit(null)} title={`Route for ${routeEdit()?.agent ?? ""} on ${routeEdit()?.node ?? ""}`}>
        <div class="space-y-3">
          <Field label="Prefer" hint="a route from the node's routes.yaml">
            <input class={inputCls} value={prefer()} onInput={(e) => setPrefer(e.currentTarget.value)} />
          </Field>
          <Field label="Then fail over to" hint="comma separated">
            <input class={inputCls} value={failover()} onInput={(e) => setFailover(e.currentTarget.value)} />
          </Field>
          <p class="text-xs text-slate-500">Live changes go to the node's overrides.yaml. "Write to YAML" makes the current override permanent in agents/{routeEdit()?.agent}.yaml.</p>
          <div class="flex flex-wrap justify-between gap-2">
            <div class="flex gap-2">
              <Button variant="ghost" onClick={() => saveRoute(true)}>Remove override</Button>
              <Button variant="ghost" onClick={() => act(() => post(`/nodes/${routeEdit()!.node}/routes/promote`, { agent: routeEdit()!.agent }), "Written to YAML")}>Write to YAML</Button>
            </div>
            <Button variant="primary" onClick={() => saveRoute(false)}>Apply live</Button>
          </div>
        </div>
      </Modal>
    </>
  );
}
