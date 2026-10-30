import { Tabs } from "@kobalte/core/tabs";
import { A } from "@solidjs/router";
import { For, Show, createResource, createSignal } from "solid-js";
import { get, post } from "../api";
import { Button, Card, Empty, Importance, NodeChip, PageHeader, Pill, act } from "../components/ui";
import PromptBox from "../components/PromptBox";
import * as I from "../icons";
import { setState, state } from "../store";
import type { Question } from "../types";
import { ago, clock } from "../util";

const MODES = [
  ["ask_me_everything", "Ask me everything", "every question, at once"],
  ["important_and_vital", "Important and vital", "normal + vital the chain could not answer"],
  ["vital_only", "Vital only", "only vital, after the chain fails"],
  ["autonomous", "Autonomous", "only credentials and budgets"],
] as const;

function QuestionCard(p: { q: Question }) {
  const [text, setText] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const answer = async (body: any) => {
    setBusy(true);
    await act(() => post(`/questions/${p.q.id}/answer`, body), "Answer delivered");
    setBusy(false);
  };
  const chain = () => [`${p.q.asker_agent} ${p.q.asker}`, ...p.q.chain.map((h) => h.holder)];
  return (
    <Card class="p-4">
      <div class="mb-2 flex flex-wrap items-center gap-2 text-xs text-slate-500">
        <Importance level={p.q.importance} />
        <Pill>{p.q.kind}</Pill>
        <Show when={p.q.operator_only}>
          <Pill tone="red">
            <I.Shield class="size-3" /> operator only
          </Pill>
        </Show>
        <span>
          blocks <b>{p.q.blocked}</b>
        </span>
        <span>{ago(p.q.created_at)} ago</span>
        <NodeChip name={p.q.node} />
        <Show when={p.q.task_id}>
          <A href="/tasks" class="underline">task #{p.q.task_id}</A>
        </Show>
      </div>
      <p class="mb-2 font-medium whitespace-pre-wrap">{p.q.text}</p>
      <Show when={p.q.context}>
        <p class="mb-2 text-sm text-slate-600 dark:text-slate-400">{p.q.context}</p>
      </Show>
      <div class="mb-2 flex flex-wrap items-center gap-1 text-xs text-slate-500" aria-label="chain">
        <For each={chain()}>
          {(c, i) => (
            <>
              <Show when={i() > 0}>
                <I.ChevronRight class="size-3" />
              </Show>
              <A href={c.startsWith("operator") ? "/questions" : `/instances/${c.split(" ").pop()}`} class="font-mono hover:underline">{c}</A>
            </>
          )}
        </For>
      </div>
      <For each={p.q.chain.filter((h) => h.note)}>
        {(h) => (
          <p class="mb-1 rounded-md bg-slate-50 px-2 py-1 text-sm dark:bg-slate-800/60">
            <span class="text-xs text-slate-500">{h.holder}:</span> {h.note}
          </p>
        )}
      </For>
      <Show when={p.q.raised_because.length}>
        <p class="mb-2 text-xs text-red-700 dark:text-red-400">raised to {p.q.importance}: {p.q.raised_because.join(", ")}</p>
      </Show>
      <div class="mt-3 flex flex-wrap gap-2">
        <For each={p.q.options}>
          {(o, i) => (
            <Button busy={busy()} onClick={() => answer({ choice: i() + 1 })} title={o.description}>
              {o.label}
            </Button>
          )}
        </For>
      </div>
      <div class="mt-3 flex flex-col gap-2 sm:flex-row">
        <PromptBox line class="flex-1" placeholder="Or type an answer…" label="answer" value={text()} onValue={setText} onSubmit={() => text().trim() && answer({ answer: text() })} />
        <div class="flex gap-2">
          <Button variant="primary" busy={busy()} disabled={!text().trim()} onClick={() => answer({ answer: text() })}>
            <I.Send class="size-4" /> Answer
          </Button>
          <Show when={!p.q.operator_only}>
            <Button busy={busy()} onClick={() => answer({ let_agent_decide: true })} title="The agent decides; logged as an assumption">
              Let it decide
            </Button>
          </Show>
          <Button variant="ghost" busy={busy()} onClick={() => answer({ reject: true })}>
            Reject
          </Button>
        </div>
      </div>
    </Card>
  );
}

export default function Questions() {
  const [history] = createResource(() => get<Question[]>("/questions?all=true").catch(() => []));
  const [assumptions] = createResource(() => get<any[]>("/assumptions").catch(() => []));
  const setMode = async (mode: string) => {
    const ok = await act(() => post("/questions/mode", { mode }), `Question mode: ${mode.replaceAll("_", " ")}`);
    if (ok && state.info) setState("info", "question_mode", mode);
  };
  return (
    <>
      <PageHeader title="Questions" subtitle="Waiting on you: most important first, then the ones blocking the most instances." />
      <Card class="mb-5 p-3">
        <div class="mb-2 text-sm font-medium">Question mode (whole cluster)</div>
        <div class="grid gap-2 sm:grid-cols-4" role="radiogroup" aria-label="question mode">
          <For each={MODES}>
            {([v, l, d]) => (
              <button
                role="radio"
                aria-checked={state.info?.question_mode === v}
                onClick={() => setMode(v)}
                class={`rounded-lg p-2 text-left text-sm ring-1 ${state.info?.question_mode === v ? "bg-indigo-600/10 ring-indigo-500" : "ring-slate-200 hover:bg-slate-50 dark:ring-slate-700 dark:hover:bg-slate-800"}`}
              >
                <div class="font-medium">{l}</div>
                <div class="text-xs text-slate-500">{d}</div>
              </button>
            )}
          </For>
        </div>
      </Card>
      <Tabs defaultValue="open">
        <Tabs.List class="mb-3 flex gap-1 border-b border-slate-200 dark:border-slate-800">
          <For each={[["open", `Open (${state.questions.length})`], ["history", "History"], ["assumptions", "Assumption log"]]}>
            {([v, l]) => (
              <Tabs.Trigger value={v} class="-mb-px border-b-2 border-transparent px-3 py-2 text-sm text-slate-600 data-[selected]:border-indigo-600 data-[selected]:text-indigo-700 dark:text-slate-400 dark:data-[selected]:text-indigo-300">
                {l}
              </Tabs.Trigger>
            )}
          </For>
        </Tabs.List>
        <Tabs.Content value="open" class="space-y-3">
          <Show when={state.questions.length} fallback={<Empty icon={I.CircleHelp} title="No questions waiting">Agents decide or ask each other first; what needs you appears here.</Empty>}>
            <For each={[...state.questions].sort((a, b) => ["low", "normal", "vital"].indexOf(b.importance) - ["low", "normal", "vital"].indexOf(a.importance) || b.blocked - a.blocked || a.created_at - b.created_at)}>
              {(q) => <QuestionCard q={q} />}
            </For>
          </Show>
        </Tabs.Content>
        <Tabs.Content value="history">
          <Card class="divide-y divide-slate-100 dark:divide-slate-800">
            <For each={(history() ?? []).filter((q) => q.status !== "open")} fallback={<p class="p-4 text-sm text-slate-500">Nothing yet.</p>}>
              {(q) => (
                <div class="p-3 text-sm">
                  <div class="mb-1 flex flex-wrap items-center gap-2 text-xs text-slate-500">
                    <Importance level={q.importance} /> <Pill>{q.status}</Pill> {q.asker_agent} · {clock(q.updated_at)} · by {q.answered_by ?? "–"}
                  </div>
                  <p>{q.text}</p>
                  <Show when={q.answer}>
                    <p class="mt-1 text-slate-600 dark:text-slate-400">→ {q.answer}</p>
                  </Show>
                </div>
              )}
            </For>
          </Card>
        </Tabs.Content>
        <Tabs.Content value="assumptions">
          <Card class="divide-y divide-slate-100 dark:divide-slate-800">
            <For each={assumptions() ?? []} fallback={<p class="p-4 text-sm text-slate-500">No decisions were made without you.</p>}>
              {(a) => (
                <div class="p-3 text-sm">
                  <div class="mb-1 text-xs text-slate-500">
                    #{a.id} · {clock(a.ts)} · {a.session} · decided by {a.decided_by} · {a.mode}
                    {a.task_id ? ` · task #${a.task_id}` : ""}
                  </div>
                  <p class="whitespace-pre-wrap">{a.text}</p>
                </div>
              )}
            </For>
          </Card>
        </Tabs.Content>
      </Tabs>
    </>
  );
}
