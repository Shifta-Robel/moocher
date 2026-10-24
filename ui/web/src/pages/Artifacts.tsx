import { A } from "@solidjs/router";
import { For, Show, createResource, createSignal } from "solid-js";
import { get, upload } from "../api";
import { Button, Card, Empty, Modal, PageHeader, Pill, act, inputCls } from "../components/ui";
import * as I from "../icons";
import type { Artifact } from "../types";
import { ago, bytes, short } from "../util";

export default function Artifacts() {
  const [q, setQ] = createSignal("");
  const [list, { refetch }] = createResource(q, (text) => get<Artifact[]>(`/artifacts?limit=200${text ? `&q=${encodeURIComponent(text)}` : ""}`).catch(() => []));
  const [sel, setSel] = createSignal<string | null>(null);
  const [detail] = createResource(sel, (r) => get<any>(`/artifacts/${encodeURIComponent(r)}`));
  let file!: HTMLInputElement;
  const send = async () => {
    const f = file.files?.[0];
    if (!f) return;
    const r = await act(() => upload<Artifact>(`/artifacts?name=${encodeURIComponent(f.name)}&kind=file`, f), "Uploaded");
    if (r) refetch();
    file.value = "";
  };
  return (
    <>
      <PageHeader
        title="Artifacts"
        subtitle="Files and trees shared between agents, with where they came from."
        actions={
          <>
            <input ref={file} type="file" class="hidden" onChange={send} />
            <Button onClick={() => file.click()}>
              <I.Upload class="size-4" /> Upload
            </Button>
          </>
        }
      />
      <div class="relative mb-4">
        <I.Search class="pointer-events-none absolute top-2.5 left-2.5 size-4 text-slate-400" />
        <input class={`${inputCls} pl-8`} placeholder="Search names and tags…" value={q()} onInput={(e) => setQ(e.currentTarget.value)} aria-label="search" />
      </div>
      <Show when={(list() ?? []).length} fallback={<Empty icon={I.Archive} title="No artifacts">Agents share files with cluster_artifact_put; harvests store outputs here too.</Empty>}>
        <Card class="divide-y divide-slate-100 dark:divide-slate-800">
          <For each={list()}>
            {(a) => (
              <button class="flex w-full flex-wrap items-center gap-2 p-3 text-left text-sm hover:bg-slate-50 dark:hover:bg-slate-800/50" onClick={() => setSel(a.reference)}>
                <Show when={a.kind === "tree"} fallback={<I.FileText class="size-4 shrink-0 text-slate-400" />}>
                  <I.Archive class="size-4 shrink-0 text-slate-400" />
                </Show>
                <span class="font-medium">{a.name}</span>
                <For each={a.tags}>{(t) => <Pill>{t}</Pill>}</For>
                <span class="ml-auto font-mono text-xs text-slate-500">{short(a.reference)}</span>
                <span class="w-16 text-right text-xs text-slate-500">{bytes(a.size)}</span>
                <span class="w-24 truncate text-right text-xs text-slate-500">{a.producer ?? "operator"}</span>
                <span class="w-10 text-right text-xs text-slate-500">{ago(a.created_at)}</span>
              </button>
            )}
          </For>
        </Card>
      </Show>
      <Modal open={!!sel()} onClose={() => setSel(null)} title={detail()?.artifact?.name ?? "Artifact"} wide>
        <Show when={detail()} fallback={<p class="text-sm text-slate-500">Loading…</p>}>
          {(d) => (
            <div class="space-y-3 text-sm">
              <p class="font-mono text-xs break-all">{d().artifact.reference}</p>
              <div class="flex flex-wrap gap-2">
                <Pill>{d().artifact.kind}</Pill>
                <Pill>{bytes(d().artifact.size)}</Pill>
                <Pill>{d().artifact.media_type}</Pill>
                <Show when={d().artifact.task_id}>
                  <Pill>task #{d().artifact.task_id}</Pill>
                </Show>
              </div>
              <p>
                made by{" "}
                <Show when={d().artifact.producer} fallback="the operator">
                  <A class="font-mono underline" href={`/instances/${d().artifact.producer}`}>{d().artifact.producer}</A>
                </Show>{" "}
                on {d().artifact.node ?? "the hub"}
              </p>
              <Show when={d().parents.length}>
                <div>
                  <div class="font-medium">made from</div>
                  <For each={d().parents}>{(p: Artifact) => <button class="block font-mono text-xs underline" onClick={() => setSel(p.reference)}>{p.name} {short(p.reference)}</button>}</For>
                </div>
              </Show>
              <Show when={d().children.length}>
                <div>
                  <div class="font-medium">used by</div>
                  <For each={d().children}>{(c: Artifact) => <button class="block font-mono text-xs underline" onClick={() => setSel(c.reference)}>{c.name} {short(c.reference)}</button>}</For>
                </div>
              </Show>
              <a class="inline-flex items-center gap-1.5 rounded-lg bg-indigo-600 px-3.5 py-2 text-sm font-medium text-white hover:bg-indigo-500" href={`/api/v1/artifacts/${d().artifact.hash}/blob`} download={d().artifact.kind === "tree" ? `${d().artifact.name}.tar.zst` : d().artifact.name}>
                <I.Download class="size-4" /> Download{d().artifact.kind === "tree" ? " (.tar.zst)" : ""}
              </a>
            </div>
          )}
        </Show>
      </Modal>
    </>
  );
}
