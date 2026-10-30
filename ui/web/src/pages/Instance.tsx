import { Tabs } from "@kobalte/core/tabs";
import { A, useParams } from "@solidjs/router";
import { For, Show, createEffect, createResource, createSignal, on } from "solid-js";
import { get, post, del } from "../api";
import PromptBox from "../components/PromptBox";
import { Button, Card, Empty, Importance, NodeChip, PageHeader, Pill, Sparkline, StatusBadge, Todos, act } from "../components/ui";
import * as I from "../icons";
import { events, isLive, state, statsTick } from "../store";
import type { LogEvent, RecoveryOption } from "../types";
import { ago, billed, bytes, clock, money, num } from "../util";

function Stat(p: { label: string; value: string; hint?: string }) {
  return (
    <div class="rounded-lg bg-slate-50 p-2.5 dark:bg-slate-800/50">
      <div class="text-xs text-slate-500">{p.label}</div>
      <div class="font-mono text-sm font-medium tabular-nums">{p.value}</div>
      <Show when={p.hint}>
        <div class="text-[11px] text-slate-500">{p.hint}</div>
      </Show>
    </div>
  );
}

/** One event: time, kind and a one-line preview; tap for the whole payload,
 * which scrolls sideways rather than being cut off. */
function ActivityRow(p: { e: LogEvent }) {
  const [open, setOpen] = createSignal(false);
  const data = () => (p.e.data == null || (typeof p.e.data === "object" && !Object.keys(p.e.data).length) ? "" : JSON.stringify(p.e.data));
  return (
    <li>
      <button class="flex w-full min-w-0 flex-col gap-0.5 p-1.5 text-left hover:bg-slate-50 sm:flex-row sm:gap-2 dark:hover:bg-slate-800/50" onClick={() => setOpen(!open())} aria-expanded={open()} disabled={!data()}>
        <span class="flex shrink-0 gap-2">
          <span class="text-slate-500">{clock(p.e.ts)}</span>
          <span class="font-semibold sm:w-36">{p.e.kind}</span>
        </span>
        <span class="min-w-0 truncate text-slate-600 dark:text-slate-400">{data()}</span>
      </button>
      <Show when={open()}>
        <pre class="mx-1.5 mb-1.5 max-h-72 overflow-auto rounded-md bg-slate-50 p-2 whitespace-pre dark:bg-slate-950">{JSON.stringify(p.e.data, null, 2)}</pre>
      </Show>
    </li>
  );
}

export default function Instance() {
  const params = useParams();
  const s = () => state.sessions.find((x) => x.id === params.id);
  const [msg, setMsg] = createSignal("");
  const [steer, setSteer] = createSignal(false);
  const [tokenSeries, setTokenSeries] = createSignal<number[]>([]);
  createEffect(on(() => s()?.tokens.input, () => { const t = s(); if (t) setTokenSeries((v) => [...v.slice(-120), t.tokens.input + t.tokens.output + t.tokens.reasoning]); }));
  const [stats, { refetch: refetchStats }] = createResource(() => params.id, (id) => get<any>(`/sessions/${id}/stats`).catch(() => null));
  createEffect(on(statsTick, () => refetchStats(), { defer: true }));
  const [log] = createResource(() => params.id, (id) => get<LogEvent[]>(`/events?session=${id}&limit=300`).catch(() => []));
  const allEvents = () => [...(log() ?? []), ...events().filter((e) => e.session === params.id && !(log() ?? []).some((x) => x.id === e.id))];
  const [preamble, setPreambleId] = createSignal<string | null>(null);
  const [pre] = createResource(preamble, (id) => get<{ preamble: string }>(`/sessions/${id}/preamble`).then((v) => v.preamble).catch((e) => `(${e.message})`));
  const [rec, { refetch: refetchRec }] = createResource(() => params.id, (id) => get<{ checkpoint: any; recovery_options: RecoveryOption[]; harvests: any[] }>(`/sessions/${id}/checkpoint`).catch(() => null));
  createEffect(on(() => s()?.state, () => refetchRec(), { defer: true }));
  const children = () => state.sessions.filter((c) => c.parent === params.id);
  const questions = () => state.questions.filter((q) => q.asker === params.id || q.holder === params.id);
  const send = async () => {
    if (!msg().trim()) return;
    await act(() => post(`/sessions/${params.id}/message`, { text: msg(), delivery: steer() ? "steer" : "queue" }), steer() ? "Sent now (steer)" : "Queued for its next step");
    setMsg("");
  };
  const openInOpenCode = async () => {
    // Open the window first (popup blockers), then point it at the ticket URL.
    const w = window.open("about:blank", "_blank");
    const r = await act(() => post<{ url: string }>(`/sessions/${params.id}/attach`));
    if (r && w) w.location.href = r.url;
    else w?.close();
  };
  const series = (k: string) => (stats()?.recent ?? []).map((x: any) => x[k] ?? 0);
  return (
    <Show when={s()} fallback={<Empty title="No such instance">It may have been cleaned up. <A class="underline" href="/instances">All instances</A></Empty>}>
      {(si) => (
        <>
          <PageHeader
            title={si().title || si().agent}
            subtitle={`${si().agent} · ${si().id}${si().task_id ? ` · task #${si().task_id}` : ""}${si().run_id ? ` · run ${si().run_id} ${si().step ?? ""}` : ""}`}
            actions={
              <>
                <Show when={state.info?.attach && isLive(si())}>
                  <Button onClick={openInOpenCode} title="OpenCode's own web UI for this instance">
                    <I.ExternalLink class="size-4" /> Open in OpenCode
                  </Button>
                </Show>
                <Show when={isLive(si())}>
                  <Button onClick={() => act(() => post(`/sessions/${si().id}/interrupt`), "Interrupted")}>
                    <I.PauseCircle class="size-4" /> Interrupt
                  </Button>
                  <Button variant="danger" onClick={() => confirm("Stop this instance? Its outputs are harvested.") && act(() => del(`/sessions/${si().id}`), "Stopped")}>
                    <I.Square class="size-4" /> Stop
                  </Button>
                </Show>
              </>
            }
          />
          <div class="mb-4 flex flex-wrap items-center gap-2">
            <StatusBadge state={si().state} questions={questions().length} />
            <NodeChip name={si().node} />
            <Pill>{si().model ?? "?"}</Pill>
            <Pill>{si().workspace ?? "?"}</Pill>
            <Show when={si().role === "coordinator"}>
              <Pill tone="indigo">coordinator</Pill>
            </Show>
            <Show when={si().verdict}>
              <Pill tone={si().verdict === "pass" ? "emerald" : "red"}>verdict {si().verdict}</Pill>
            </Show>
            <Show when={si().attached_by && (si().attached_until ?? 0) > Date.now()}>
              <Pill tone="indigo">
                <I.ExternalLink class="size-3" /> attached by {si().attached_by}
              </Pill>
            </Show>
            <Show when={(si().deaths ?? 0) > 0 || si().replaces}>
              <Pill tone="orange">
                {si().deaths ? `recovered ${si().deaths}×` : ""} {si().replaces ? `replaces ${si().replaces}` : ""}
              </Pill>
            </Show>
          </div>
          <Show when={si().error}>
            <Card class="mb-4 p-3 ring-red-300 dark:ring-red-800">
              <p class="text-sm text-red-700 dark:text-red-400">
                <I.AlertTriangle class="mr-1 inline size-4" />
                {si().error}
              </p>
            </Card>
          </Show>
          <Show when={["dead", "lost"].includes(si().state) && rec()?.recovery_options}>
            <Card class="mb-4 p-4">
              <h2 class="mb-2 font-semibold">Recover</h2>
              <div class="grid gap-2 sm:grid-cols-2">
                <For each={rec()!.recovery_options}>
                  {(o) => (
                    <Button disabled={!o.available} onClick={() => act(() => post(`/sessions/${si().id}/recover`, { option: o.kind }), `${o.kind} started`)} class="justify-start text-left" title={o.note}>
                      <I.RotateCcw class="size-4 shrink-0" />
                      <span class="flex-1">
                        {o.kind.replace("_", " ")} <span class="text-xs text-slate-500">~{money(o.est_cost_usd)}</span>
                        <span class="block text-xs font-normal text-slate-500">{o.note}</span>
                      </span>
                    </Button>
                  )}
                </For>
              </div>
            </Card>
          </Show>
          <div class="mb-4 grid grid-cols-2 gap-2 sm:grid-cols-4 lg:grid-cols-6">
            <Stat label="tokens" value={num(billed(si()).total)} hint={`in ${num(billed(si()).input)} · out ${num(billed(si()).output)}`} />
            <Stat label="cost" value={money(billed(si()).cost_usd)} hint={si().metered && billed(si()).total !== si().tokens.input + si().tokens.output + si().tokens.reasoning ? `metered; OpenCode says ${num(si().tokens.input + si().tokens.output + si().tokens.reasoning)} tok` : undefined} />
            <Stat label="context" value={num(si().context_tokens ?? 0)} hint="last request" />
            <Stat label="turns" value={`${si().turns ?? 0}`} />
            <Stat label="CPU" value={stats()?.latest?.cpu_pct != null ? `${stats().latest.cpu_pct.toFixed(1)}%` : "–"} />
            <Stat label="memory" value={bytes(stats()?.latest?.mem_bytes)} hint={stats()?.latest?.mem_limit ? `of ${bytes(stats().latest.mem_limit)}` : undefined} />
          </div>
          <Show when={isLive(si())}>
            <Card class="mb-4 p-3">
              <div class="flex flex-col gap-2 sm:flex-row">
                <PromptBox class="flex-1" rows={2} placeholder="Message this agent…" label="message" value={msg()} onValue={setMsg} onSubmit={send} />
                <div class="flex items-center gap-2 sm:flex-col sm:items-stretch">
                  <label class="inline-flex items-center gap-1.5 text-xs" title="steer: abort the current step and deliver now; otherwise it waits for the next step">
                    <input type="checkbox" checked={steer()} onChange={(e) => setSteer(e.currentTarget.checked)} /> deliver now (steer)
                  </label>
                  <Button variant="primary" onClick={send}>
                    <I.Send class="size-4" /> Send
                  </Button>
                </div>
              </div>
            </Card>
          </Show>
          <Tabs defaultValue="todos" onChange={(v) => v === "preamble" && setPreambleId(si().id)}>
            <Tabs.List class="mb-3 flex gap-1 overflow-x-auto border-b border-slate-200 dark:border-slate-800">
              <For each={[["todos", "Todos"], ["activity", "Activity"], ["usage", "Usage"], ["chain", "Chain"], ["preamble", "What it was told"], ["placement", "Placement"], ["outputs", "Outputs"]]}>
                {([v, l]) => (
                  <Tabs.Trigger value={v} class="-mb-px border-b-2 border-transparent px-3 py-2 text-sm whitespace-nowrap text-slate-600 data-[selected]:border-indigo-600 data-[selected]:text-indigo-700 dark:text-slate-400 dark:data-[selected]:text-indigo-300">
                    {l}
                  </Tabs.Trigger>
                )}
              </For>
            </Tabs.List>
            <Tabs.Content value="todos">
              <Card class="p-4">
                <Todos todos={si().todos} />
                <Show when={si().last_text}>
                  <h3 class="mt-4 mb-1 text-sm font-semibold">Last output</h3>
                  <pre class="max-h-80 overflow-auto rounded-lg bg-slate-50 p-3 text-xs whitespace-pre-wrap dark:bg-slate-950">{si().last_text}</pre>
                </Show>
              </Card>
            </Tabs.Content>
            <Tabs.Content value="activity">
              <Card class="max-h-[32rem] overflow-auto p-2">
                <ul class="divide-y divide-slate-100 font-mono text-xs dark:divide-slate-800">
                  <For each={[...allEvents()].reverse()} fallback={<li class="p-2 text-slate-500">No events yet.</li>}>
                    {(e) => <ActivityRow e={e} />}
                  </For>
                </ul>
              </Card>
            </Tabs.Content>
            <Tabs.Content value="usage">
              <div class="grid gap-3 md:grid-cols-2">
                <Card class="p-3 text-indigo-600 dark:text-indigo-400">
                  <div class="text-xs text-slate-500">tokens (while this page is open)</div>
                  <Sparkline values={tokenSeries()} label="tokens over time" />
                </Card>
                <Card class="p-3 text-emerald-600 dark:text-emerald-400">
                  <div class="text-xs text-slate-500">CPU % (last hour)</div>
                  <Sparkline values={series("cpu_pct")} label="cpu over time" />
                </Card>
                <Card class="p-3 text-sky-600 dark:text-sky-400">
                  <div class="text-xs text-slate-500">memory (last hour)</div>
                  <Sparkline values={series("mem_bytes")} label="memory over time" />
                </Card>
                <Card class="p-3 text-amber-600 dark:text-amber-400">
                  <div class="text-xs text-slate-500">disk {bytes(stats()?.latest?.disk_bytes)}{stats()?.latest?.disk_limit ? ` of ${bytes(stats().latest.disk_limit)}` : ""}</div>
                  <Sparkline values={series("disk_bytes")} label="disk over time" />
                </Card>
              </div>
            </Tabs.Content>
            <Tabs.Content value="chain">
              <Card class="space-y-3 p-4 text-sm">
                <div>
                  answers to:{" "}
                  <Show when={si().parent} fallback={<span class="text-slate-500">the operator</span>}>
                    <A class="font-mono text-indigo-600 underline dark:text-indigo-400" href={`/instances/${si().parent}`}>{si().parent}</A>
                  </Show>
                </div>
                <div>
                  started:{" "}
                  <For each={children()} fallback={<span class="text-slate-500">none</span>}>
                    {(c) => (
                      <A class="mr-2 font-mono text-indigo-600 underline dark:text-indigo-400" href={`/instances/${c.id}`}>
                        {c.agent} {c.id}
                      </A>
                    )}
                  </For>
                </div>
                <For each={questions()}>
                  {(q) => (
                    <div class="flex items-center gap-2">
                      <Importance level={q.importance} />
                      <A href="/questions" class="truncate underline">{q.text}</A>
                      <span class="text-xs text-slate-500">held by {q.holder}</span>
                    </div>
                  )}
                </For>
              </Card>
            </Tabs.Content>
            <Tabs.Content value="preamble">
              <Card class="p-3">
                <pre class="max-h-[32rem] overflow-auto text-xs whitespace-pre-wrap">{pre.loading ? "Loading…" : pre() ?? ""}</pre>
              </Card>
            </Tabs.Content>
            <Tabs.Content value="placement">
              <Card class="p-3 text-sm">
                <p class="break-words">{si().placement ?? "—"}</p>
                <p class="mt-2 text-xs text-slate-500">started {ago(si().started_at)} ago · updated {ago(si().updated_at)} ago</p>
              </Card>
            </Tabs.Content>
            <Tabs.Content value="outputs">
              <Card class="space-y-3 p-3 text-sm">
                <Show when={rec()?.checkpoint}>
                  <div>
                    checkpoint after turn {rec()!.checkpoint.turn} ({ago(rec()!.checkpoint.at)} ago):{" "}
                    <span class="font-mono text-xs">{rec()!.checkpoint.workspace_ref ?? "–"}</span>
                  </div>
                </Show>
                <For each={rec()?.harvests ?? []} fallback={<p class="text-slate-500">Not harvested yet (outputs are collected when it ends).</p>}>
                  {(h) => (
                    <div>
                      <div class="font-medium">
                        harvest ({h.trigger}) {clock(h.at)} {h.from_checkpoint ? "· from checkpoint" : ""}
                      </div>
                      <ul class="mt-1 space-y-0.5 font-mono text-xs">
                        <For each={h.outputs}>
                          {(o: any) => (
                            <li>
                              {o.path} → {o.to}{" "}
                              <a class="text-indigo-600 underline dark:text-indigo-400" href={`/api/v1/artifacts/${o.reference.slice(9)}/blob`}>
                                download
                              </a>{" "}
                              <span class="text-slate-500">{o.delivered ?? ""}</span>
                            </li>
                          )}
                        </For>
                        <For each={h.missing}>{(m: string) => <li class="text-slate-500">{m} (matched nothing)</li>}</For>
                        <For each={h.notes}>{(n: string) => <li class="text-amber-700 dark:text-amber-400">{n}</li>}</For>
                      </ul>
                    </div>
                  )}
                </For>
              </Card>
            </Tabs.Content>
          </Tabs>
        </>
      )}
    </Show>
  );
}
