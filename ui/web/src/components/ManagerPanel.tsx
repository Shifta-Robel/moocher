// Ask the manager about the cluster. The conversation lives only here, in this
// tab: the hub keeps nothing between questions, and gets just the last few
// questions and answers as text, so that "and the other one?" makes sense.
import { A, useLocation, useNavigate } from "@solidjs/router";
import { For, Show, createSignal } from "solid-js";
import { get, post } from "../api";
import * as I from "../icons";
import { cx, money } from "../util";
import PromptBox from "./PromptBox";
import { Button } from "./ui";

interface Link { path: string; label: string }
interface Msg { role: "user" | "assistant"; text: string; links?: Link[]; cost?: number; error?: boolean }
interface Status { configured: boolean; ready: boolean; agent?: string; why?: string | null }

export default function ManagerPanel() {
  const nav = useNavigate();
  const loc = useLocation();
  const [open, setOpen] = createSignal(false);
  const [status, setStatus] = createSignal<Status | null>(null);
  const [msgs, setMsgs] = createSignal<Msg[]>([]);
  const [text, setText] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  let end: HTMLDivElement | undefined;
  const down = () => queueMicrotask(() => end?.scrollIntoView({ block: "end" }));
  const toggle = async () => {
    setOpen(!open());
    if (open()) setStatus(await get<Status>("/manager").catch((e) => ({ configured: false, ready: false, why: String(e.message ?? e) })));
  };
  const ask = async () => {
    const question = text().trim();
    if (!question || busy()) return;
    const history = msgs().filter((m) => !m.error).map((m) => ({ role: m.role, text: m.text }));
    setMsgs([...msgs(), { role: "user", text: question }]);
    setText("");
    setBusy(true);
    down();
    try {
      const r = await post<{ answer: string; links: Link[]; navigate?: string | null; usage: { cost_usd: number } }>("/manager/ask", { question, history, page: loc.pathname });
      setMsgs([...msgs(), { role: "assistant", text: r.answer, links: r.links, cost: r.usage.cost_usd }]);
      if (r.navigate) nav(r.navigate);
    } catch (e: any) {
      setMsgs([...msgs(), { role: "assistant", text: e.message ?? String(e), error: true }]);
    }
    setBusy(false);
    down();
  };
  return (
    <>
      <button
        class="fixed right-4 bottom-[calc(5rem+env(safe-area-inset-bottom))] z-40 flex size-12 items-center justify-center rounded-full bg-indigo-600 text-white shadow-lg hover:bg-indigo-500 md:right-6 md:bottom-6"
        onClick={toggle}
        aria-label={open() ? "close the manager" : "ask the manager"}
        aria-expanded={open()}
        title="Ask the manager"
      >
        <Show when={open()} fallback={<I.MessageCircle class="size-6" />}><I.X class="size-6" /></Show>
      </button>
      <Show when={open()}>
        <section class="fixed right-4 bottom-[calc(8.5rem+env(safe-area-inset-bottom))] z-40 flex h-[min(32rem,calc(100dvh-11rem))] w-[calc(100vw-2rem)] flex-col rounded-2xl bg-white shadow-2xl ring-1 ring-slate-200 md:right-6 md:bottom-22 md:w-96 dark:bg-slate-900 dark:ring-slate-700" aria-label="manager">
          <header class="flex items-center justify-between border-b border-slate-200 px-3 py-2 dark:border-slate-800">
            <div>
              <h2 class="text-sm font-semibold">Manager</h2>
              <p class="text-xs text-slate-500">Reads the cluster live; changes nothing</p>
            </div>
            <Button size="sm" variant="ghost" onClick={() => setMsgs([])} disabled={!msgs().length}>Clear</Button>
          </header>
          <div class="min-h-0 flex-1 space-y-2 overflow-y-auto p-3" aria-live="polite">
            <Show when={status() && !status()!.ready}>
              <p class="rounded-lg bg-amber-100 p-2 text-sm text-amber-900 dark:bg-amber-950 dark:text-amber-300">{status()!.why}</p>
            </Show>
            <Show when={status()?.ready && !msgs().length}>
              <p class="text-sm text-slate-500">Try "what is running on each node?", "why is that session paused?" or "take me to the quotas".</p>
            </Show>
            <For each={msgs()}>
              {(m) => (
                <div class={cx("max-w-[90%] rounded-xl px-3 py-2 text-sm break-words whitespace-pre-wrap", m.role === "user" ? "ml-auto bg-indigo-600 text-white" : m.error ? "bg-red-100 text-red-800 dark:bg-red-950 dark:text-red-300" : "bg-slate-100 dark:bg-slate-800")}>
                  {m.text}
                  <Show when={m.links?.length}>
                    <div class="mt-1.5 flex flex-wrap gap-1.5">
                      <For each={m.links}>{(l) => <A href={l.path} class="inline-flex items-center gap-1 rounded-md bg-white px-2 py-0.5 text-xs font-medium text-indigo-700 ring-1 ring-slate-200 dark:bg-slate-900 dark:text-indigo-300 dark:ring-slate-700"><I.ExternalLink class="size-3" />{l.label}</A>}</For>
                    </div>
                  </Show>
                  <Show when={m.cost}><div class="mt-1 text-[10px] text-slate-500">{money(m.cost!)}</div></Show>
                </div>
              )}
            </For>
            <Show when={busy()}><p class="text-sm text-slate-500">Looking…</p></Show>
            <div ref={end} />
          </div>
          <form class="flex gap-2 border-t border-slate-200 p-2 dark:border-slate-800" onSubmit={(e) => { e.preventDefault(); ask(); }}>
            <PromptBox line class="flex-1" value={text()} onValue={setText} onSubmit={ask} label="ask the manager" placeholder="Ask about the cluster…" />
            <Button variant="primary" type="submit" disabled={busy() || !text().trim() || !status()?.ready} aria-label="send"><I.Send class="size-4" /></Button>
          </form>
        </section>
      </Show>
    </>
  );
}
