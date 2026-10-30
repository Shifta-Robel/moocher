import { A } from "@solidjs/router";
import { For, Show, createSignal } from "solid-js";
import { del, get, post } from "../api";
import { Button, Card, Empty, Field, Modal, PageHeader, Pill, act, inputCls } from "../components/ui";
import PromptBox from "../components/PromptBox";
import * as I from "../icons";
import { setState, state } from "../store";
import type { Task } from "../types";
import { ago, money } from "../util";

const LABELS = [
  ["urgent", "Urgent", "starts as soon as a slot is free"],
  ["when_free", "When free", "starts when the cluster is idle"],
  ["on_demand", "On demand", "starts when you press Run"],
] as const;

function statusTone(s: string) {
  return ({ running: "emerald", done: "slate", failed: "red", cancelled: "slate", halted: "orange", queued: "sky" } as const)[s as "running"] ?? "slate";
}

export default function Tasks() {
  const [adding, setAdding] = createSignal(false);
  const [title, setTitle] = createSignal("");
  const [body, setBody] = createSignal("");
  const [label, setLabel] = createSignal("when_free");
  const [tags, setTags] = createSignal("");
  const [qmode, setQmode] = createSignal("");
  const [dragging, setDragging] = createSignal<number | null>(null);
  const [runWf, setRunWf] = createSignal<string | null>(null);
  const [wfInput, setWfInput] = createSignal("");
  const [workflows, setWorkflows] = createSignal<any[]>([]);
  get<any[]>("/workflows").then(setWorkflows).catch(() => {});
  const open = () => state.tasks.filter((t) => t.status === "queued" || t.status === "running" || t.status === "halted");
  const done = () => state.tasks.filter((t) => !open().includes(t)).slice(0, 30);
  const queued = (l: string) => state.tasks.filter((t) => t.status === "queued" && t.label === l).sort((a, b) => a.rank - b.rank);
  const reorder = async (ids: number[]) => {
    const r = await act(() => post<Task[]>("/tasks/reorder", { ids }));
    if (r) setState("tasks", r);
  };
  const dropOn = (target: Task) => {
    const from = dragging();
    setDragging(null);
    if (from == null || from === target.id) return;
    const order = state.tasks.filter((t) => t.status === "queued").sort((a, b) => a.rank - b.rank).map((t) => t.id).filter((id) => id !== from);
    order.splice(order.indexOf(target.id), 0, from);
    reorder(order);
  };
  const move = (t: Task, dir: -1 | 1) => {
    const order = state.tasks.filter((x) => x.status === "queued").sort((a, b) => a.rank - b.rank).map((x) => x.id);
    const i = order.indexOf(t.id);
    const j = i + dir;
    if (j < 0 || j >= order.length) return;
    [order[i], order[j]] = [order[j], order[i]];
    reorder(order);
  };
  const add = async () => {
    const r = await act(
      () => post("/tasks", { title: title(), body: body(), label: label(), tags: tags().split(",").map((s) => s.trim()).filter(Boolean), question_mode: qmode() || undefined }),
      "Task added",
    );
    if (r) {
      setAdding(false);
      setTitle("");
      setBody("");
      setTags("");
    }
  };
  return (
    <>
      <PageHeader
        title="Tasks & workflows"
        subtitle="Drag to reorder (or use the arrows). Within a label, higher runs first."
        actions={
          <Button variant="primary" onClick={() => setAdding(true)}>
            <I.Plus class="size-4" /> Add task
          </Button>
        }
      />
      <Show when={state.halts.length}>
        <h2 class="mb-2 font-semibold text-orange-700 dark:text-orange-400">Halted — choose how to continue</h2>
        <div class="mb-6 space-y-3">
          <For each={state.halts}>
            {(h) => (
              <Card class="p-4 ring-orange-300 dark:ring-orange-800">
                <div class="mb-1 flex flex-wrap items-center gap-2">
                  <I.PauseCircle class="size-5 text-orange-500" />
                  <span class="font-semibold">{h.scope}</span>
                  <span class="text-sm text-slate-500">
                    <A href={`/instances/${h.dead_session}`} class="font-mono underline">{h.dead_session}</A> died ({h.reason}) {ago(h.at)} ago · spent {money(h.spent_usd)}
                  </span>
                </div>
                <Show when={h.current_todo}>
                  <p class="mb-2 text-sm">was doing: {h.current_todo}</p>
                </Show>
                <Show when={h.sessions.length}>
                  <p class="mb-2 text-xs text-slate-500">paused (containers kept): {h.sessions.join(", ")}</p>
                </Show>
                <div class="grid gap-2 sm:grid-cols-2 lg:grid-cols-4">
                  <For each={h.options}>
                    {(o) => (
                      <Button
                        variant={o.kind === "abandon" ? "default" : "primary"}
                        disabled={!o.available}
                        class="flex-col items-start text-left"
                        title={o.note}
                        onClick={() => (o.kind !== "abandon" || confirm("Abandon? The task fails.")) && act(() => post(`/halts/${h.id}/recover`, { option: o.kind }), `${o.kind.replace("_", " ")}: resuming`)}
                      >
                        <span>
                          {o.kind.replace("_", " ")} · ~{money(o.est_cost_usd)}
                        </span>
                        <span class="text-xs font-normal opacity-80">{o.note}</span>
                      </Button>
                    )}
                  </For>
                </div>
              </Card>
            )}
          </For>
        </div>
      </Show>
      <Show when={state.tasks.some((t) => t.status === "running")}>
        <h2 class="mb-2 font-semibold">Running</h2>
        <div class="mb-6 grid gap-3 md:grid-cols-2">
          <For each={state.tasks.filter((t) => t.status === "running")}>
            {(t) => (
              <Card class="min-w-0 p-3">
                <div class="flex items-start justify-between gap-2">
                  <div class="min-w-0">
                    <div class="font-medium break-words">#{t.id} {t.title}</div>
                    <div class="text-xs text-slate-500">started {ago(t.started_at ?? t.created_at)} ago</div>
                  </div>
                  <Button size="sm" variant="ghost" onClick={() => confirm(`Cancel task #${t.id}?`) && act(() => post(`/tasks/${t.id}/cancel`), "Cancelled")}>
                    Cancel
                  </Button>
                </div>
                <Show when={t.coordinator_session}>
                  <A href={`/instances/${t.coordinator_session}`} class="text-xs text-indigo-600 underline dark:text-indigo-400">coordinator {t.coordinator_session}</A>
                </Show>
                <For each={t.notes.slice(-3)}>{(n) => <p class="mt-1 truncate text-xs text-slate-500">{n}</p>}</For>
              </Card>
            )}
          </For>
        </div>
      </Show>
      <div class="grid gap-4 lg:grid-cols-3">
        <For each={LABELS}>
          {([l, name, desc]) => (
            <section class="min-w-0">
              <h2 class="font-semibold">{name}</h2>
              <p class="mb-2 text-xs text-slate-500">{desc}</p>
              <div class="space-y-2" onDragOver={(e) => e.preventDefault()}>
                <For each={queued(l)} fallback={<p class="rounded-lg border border-dashed border-slate-300 p-3 text-sm text-slate-500 dark:border-slate-700">empty</p>}>
                  {(t) => (
                    <div draggable={true} onDragStart={() => setDragging(t.id)} onDragOver={(e) => e.preventDefault()} onDrop={() => dropOn(t)} class={dragging() === t.id ? "opacity-50" : ""}>
                      <Card class="flex items-start gap-2 p-2.5">
                        <I.GripVertical class="mt-0.5 size-4 shrink-0 cursor-grab text-slate-400" aria-hidden="true" />
                        <div class="min-w-0 flex-1">
                          <div class="line-clamp-3 text-sm font-medium break-words" title={t.title}>#{t.id} {t.title}</div>
                          <div class="flex flex-wrap gap-1">
                            <For each={t.tags}>{(g) => <Pill>{g}</Pill>}</For>
                            <Show when={t.question_mode}>
                              <Pill tone="indigo">{t.question_mode}</Pill>
                            </Show>
                          </div>
                        </div>
                        <div class="flex shrink-0 items-center">
                          <button class="rounded p-1 text-slate-500 hover:bg-slate-100 dark:hover:bg-slate-800" onClick={() => move(t, -1)} aria-label="move up">▲</button>
                          <button class="rounded p-1 text-slate-500 hover:bg-slate-100 dark:hover:bg-slate-800" onClick={() => move(t, 1)} aria-label="move down">▼</button>
                          <Button size="sm" variant="ghost" onClick={() => act(() => post(`/tasks/${t.id}/run`), `Task #${t.id} started`)} aria-label="run now" title="run now">
                            <I.Play class="size-3.5" />
                          </Button>
                          <Button size="sm" variant="ghost" onClick={() => confirm(`Delete task #${t.id}?`) && act(() => del(`/tasks/${t.id}`), "Deleted")} aria-label="delete" title="delete">
                            <I.Trash class="size-3.5" />
                          </Button>
                        </div>
                      </Card>
                    </div>
                  )}
                </For>
              </div>
            </section>
          )}
        </For>
      </div>
      <h2 class="mt-8 mb-2 font-semibold">Workflows</h2>
      <div class="mb-4 flex flex-wrap gap-2">
        <For each={workflows()} fallback={<span class="text-sm text-slate-500">No workflows in the hub's workflows/.</span>}>
          {(w) => (
            <Button onClick={() => setRunWf(w.name)} title={w.description}>
              <I.Play class="size-4" /> {w.name}
            </Button>
          )}
        </For>
      </div>
      <Card class="mb-6 divide-y divide-slate-100 dark:divide-slate-800">
        <For each={state.runs.slice(0, 20)} fallback={<p class="p-3 text-sm text-slate-500">No runs yet.</p>}>
          {(r) => (
            <div class="flex flex-wrap items-center gap-2 p-3 text-sm">
              <span class="font-mono text-xs">{r.id}</span>
              <span class="font-medium">{r.workflow}</span>
              <Pill tone={r.status === "passed" ? "emerald" : r.status === "running" ? "sky" : r.status === "halted" ? "orange" : "red"}>{r.status}</Pill>
              <span class="flex flex-wrap items-center gap-1 text-xs">
                <For each={r.steps}>
                  {(st, i) => (
                    <>
                      <Show when={i() > 0}>
                        <I.ChevronRight class="size-3 text-slate-400" />
                      </Show>
                      <A href={`/instances/${st.session}`} class="underline">{st.step}</A>
                      <span class={st.verdict === "pass" ? "text-emerald-600" : st.verdict ? "text-red-600" : "text-slate-500"}>{st.verdict ?? "…"}</span>
                    </>
                  )}
                </For>
              </span>
              <span class="ml-auto text-xs text-slate-500">{ago(r.created_at)}</span>
              <Show when={r.status === "running"}>
                <Button size="sm" variant="ghost" onClick={() => act(() => post(`/runs/${r.id}/cancel`), "Run cancelled")}>
                  Cancel
                </Button>
              </Show>
            </div>
          )}
        </For>
      </Card>
      <h2 class="mb-2 font-semibold">Finished tasks</h2>
      <Show when={done().length} fallback={<Empty icon={I.ListChecks} title="Nothing finished yet" />}>
        <Card class="divide-y divide-slate-100 dark:divide-slate-800">
          <For each={done()}>
            {(t) => (
              <div class="flex items-start gap-2 p-3 text-sm">
                <Pill tone={statusTone(t.status)}>{t.status}</Pill>
                <div class="min-w-0 flex-1">
                  <div class="font-medium break-words">#{t.id} {t.title}</div>
                  <Show when={t.result}>
                    <p class="line-clamp-2 text-xs text-slate-500">{t.result}</p>
                  </Show>
                </div>
                <span class="text-xs text-slate-500">{ago(t.finished_at ?? t.created_at)}</span>
              </div>
            )}
          </For>
        </Card>
      </Show>
      <Modal open={adding()} onClose={() => setAdding(false)} title="Add a task">
        <form class="space-y-3" onSubmit={(e) => { e.preventDefault(); add(); }}>
          <Field label="Title">
            <input class={inputCls} value={title()} onInput={(e) => setTitle(e.currentTarget.value)} required />
          </Field>
          <Field label="Details">
            <PromptBox rows={4} label="details" value={body()} onValue={setBody} />
          </Field>
          <Field label="When">
            <select class={inputCls} value={label()} onChange={(e) => setLabel(e.currentTarget.value)}>
              <For each={LABELS}>{([v, n, d]) => <option value={v}>{n} — {d}</option>}</For>
            </select>
          </Field>
          <Field label="Tags" hint="comma separated; workflow:<name> runs that workflow">
            <input class={inputCls} value={tags()} onInput={(e) => setTags(e.currentTarget.value)} />
          </Field>
          <Field label="Question mode for this task">
            <select class={inputCls} value={qmode()} onChange={(e) => setQmode(e.currentTarget.value)}>
              <option value="">cluster default</option>
              <option value="ask_me_everything">ask me everything</option>
              <option value="important_and_vital">important and vital</option>
              <option value="vital_only">vital only</option>
              <option value="autonomous">autonomous</option>
            </select>
          </Field>
          <div class="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => setAdding(false)}>Cancel</Button>
            <Button type="submit" variant="primary">Add</Button>
          </div>
        </form>
      </Modal>
      <Modal open={!!runWf()} onClose={() => setRunWf(null)} title={`Run ${runWf() ?? ""}`}>
        <form class="space-y-3" onSubmit={async (e) => { e.preventDefault(); const r = await act(() => post(`/workflows/${runWf()}/run`, { input: wfInput() }), "Run started"); if (r) setRunWf(null); }}>
          <Field label="Input" hint="becomes {{input}} in the steps' prompts">
            <PromptBox rows={4} label="input" value={wfInput()} onValue={setWfInput} required />
          </Field>
          <div class="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => setRunWf(null)}>Cancel</Button>
            <Button type="submit" variant="primary">Run</Button>
          </div>
        </form>
      </Modal>
    </>
  );
}
