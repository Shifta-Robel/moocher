import { For, Show, createResource, createSignal } from "solid-js";
import { get, post } from "../api";
import { Button, Card, Empty, Field, IpChip, Modal, NodeChip, PageHeader, Pill, QuotaBar, act, inputCls } from "../components/ui";
import * as I from "../icons";
import { refreshQuotas, state } from "../store";
import type { Quota } from "../types";

export default function Quotas() {
  const [egress] = createResource(() => get<any>("/egress").catch(() => ({ ips: [], shared: [] })));
  const [edit, setEdit] = createSignal<Quota | null>(null);
  const [remaining, setRemaining] = createSignal("");
  const [resets, setResets] = createSignal("");
  const providers = () => [...new Set(state.quotas.map((q) => q.provider))].sort();
  const sharers = (ip: string) => egress()?.shared?.find((s: any) => s.ip === ip)?.nodes?.length ?? 1;
  const save = async (clear = false) => {
    const q = edit()!;
    const at = resets() ? new Date(resets()).getTime() : undefined;
    const r = await act(
      () => post("/quotas/override", { provider: q.provider, scope_id: q.scope_id, remaining: remaining() === "" ? undefined : Number(remaining()), resets_at: at, clear, note: "set from the dashboard" }),
      clear ? "Override removed" : "Override saved on the node(s)",
    );
    if (r) {
      setEdit(null);
      setTimeout(() => refreshQuotas(), 800);
    }
  };
  return (
    <>
      <PageHeader title="Providers & quotas" subtitle="Allowances per IP, key or account, with reset countdowns. Overrides are written to the node's overrides.yaml." />
      <Show when={state.quotas.length} fallback={<Empty icon={I.Gauge} title="No allowances seen yet">They appear after the first provider calls.</Empty>}>
        <div class="space-y-5">
          <For each={providers()}>
            {(p) => (
              <Card class="p-4">
                <h2 class="mb-3 font-semibold">{p}</h2>
                <div class="space-y-3">
                  <For each={state.quotas.filter((q) => q.provider === p).sort((a, b) => a.scope_id.localeCompare(b.scope_id) || a.kind.localeCompare(b.kind))}>
                    {(q) => (
                      <div>
                        <div class="mb-1 flex flex-wrap items-center gap-2 text-xs">
                          <Show when={q.scope === "ip"} fallback={<Pill>{q.scope_id}</Pill>}>
                            <IpChip ip={q.scope_id} sharers={sharers(q.scope_id)} />
                          </Show>
                          <Pill>{q.kind}</Pill>
                          <Pill tone={q.source === "override" ? "indigo" : "slate"} title="where the numbers come from">{q.source}</Pill>
                          <For each={q.nodes}>{(n) => <NodeChip name={n} />}</For>
                          <span class="text-slate-500">{q.sessions.length ? `used by ${q.sessions.length}` : ""}</span>
                          <Button size="sm" variant="ghost" class="ml-auto" onClick={() => { setRemaining(""); setResets(""); setEdit(q); }}>
                            override
                          </Button>
                        </div>
                        <QuotaBar used={q.used} limit={q.limit} remaining={q.remaining} exhausted={q.exhausted} resets={q.resets_at} />
                      </div>
                    )}
                  </For>
                </div>
              </Card>
            )}
          </For>
        </div>
      </Show>
      <h2 class="mt-8 mb-2 font-semibold">Egress IPs</h2>
      <Card class="overflow-x-auto">
        <table class="w-full text-sm">
          <thead class="text-left text-xs text-slate-500">
            <tr>
              <th class="p-2">node</th>
              <th class="p-2">route</th>
              <th class="p-2">IP</th>
              <th class="p-2">learned from</th>
            </tr>
          </thead>
          <tbody>
            <For each={egress()?.ips ?? []}>
              {(e: any) => (
                <tr class="border-t border-slate-100 dark:border-slate-800">
                  <td class="p-2"><NodeChip name={e.node} /></td>
                  <td class="p-2 font-mono text-xs">{e.route}</td>
                  <td class="p-2"><IpChip ip={e.ip} sharers={e.ip ? sharers(e.ip) : 1} /></td>
                  <td class="p-2 text-xs text-slate-500">{e.source}{e.error ? ` · ${e.error}` : ""}</td>
                </tr>
              )}
            </For>
          </tbody>
        </table>
      </Card>
      <Modal open={!!edit()} onClose={() => setEdit(null)} title={`Override ${edit()?.provider ?? ""} · ${edit()?.scope_id ?? ""}`}>
        <div class="space-y-3">
          <Field label="Remaining requests" hint="0 = used up">
            <input class={inputCls} type="number" min="0" value={remaining()} onInput={(e) => setRemaining(e.currentTarget.value)} />
          </Field>
          <Field label="Resets at (your local time)">
            <input class={inputCls} type="datetime-local" value={resets()} onInput={(e) => setResets(e.currentTarget.value)} />
          </Field>
          <div class="flex justify-between gap-2">
            <Button variant="ghost" onClick={() => save(true)}>Remove override</Button>
            <Button variant="primary" onClick={() => save(false)}>Save</Button>
          </div>
        </div>
      </Modal>
    </>
  );
}
