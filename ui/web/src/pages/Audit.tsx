import { For, Show, createResource, createSignal } from "solid-js";
import { get } from "../api";
import { Button, Card, PageHeader, Pill, act } from "../components/ui";
import * as I from "../icons";
import { clock } from "../util";

export default function Audit() {
  const [list] = createResource(() => get<any[]>("/audit?limit=500").catch((e) => [{ error: e.message }]));
  const [verified, setVerified] = createSignal<any>(null);
  return (
    <>
      <PageHeader
        title="Audit log"
        subtitle="Every control action, hash-chained (tampering breaks the chain)."
        actions={
          <Button onClick={async () => setVerified(await act(() => get("/audit/verify")))}>
            <I.Shield class="size-4" /> Verify chain
          </Button>
        }
      />
      <Show when={verified()}>
        <p class={`mb-3 text-sm ${verified().ok ? "text-emerald-700 dark:text-emerald-400" : "text-red-700"}`}>{verified().ok ? `Chain intact: ${verified().entries} entries.` : verified().error}</p>
      </Show>
      <Card class="divide-y divide-slate-100 sm:hidden dark:divide-slate-800">
        <For each={list() ?? []}>
          {(e) => (
            <Show when={!e.error} fallback={<p class="p-3 text-sm text-red-600">{e.error}</p>}>
              <div class="p-3 text-sm">
                <div class="flex items-center justify-between gap-2">
                  <span class="font-medium">{e.action}</span>
                  {e.ok ? <Pill tone="emerald">ok</Pill> : <Pill tone="red">failed</Pill>}
                </div>
                <div class="mt-0.5 font-mono text-xs break-all text-slate-500">
                  {clock(e.ts)} · {e.actor}
                  <Show when={e.target && e.target !== "-"}> · {e.target}</Show>
                </div>
              </div>
            </Show>
          )}
        </For>
      </Card>
      <Card class="hidden overflow-x-auto sm:block">
        <table class="w-full text-sm">
          <thead class="text-left text-xs text-slate-500">
            <tr>
              <th class="p-2">time</th>
              <th class="p-2">who</th>
              <th class="p-2">action</th>
              <th class="p-2">target</th>
              <th class="p-2">result</th>
            </tr>
          </thead>
          <tbody>
            <For each={list() ?? []}>
              {(e) => (
                <Show when={!e.error} fallback={<tr><td class="p-2 text-red-600">{e.error}</td></tr>}>
                  <tr class="border-t border-slate-100 dark:border-slate-800" title={JSON.stringify(e.detail)}>
                    <td class="p-2 font-mono text-xs whitespace-nowrap">{clock(e.ts)}</td>
                    <td class="p-2 font-mono text-xs">{e.actor}</td>
                    <td class="p-2">{e.action}</td>
                    <td class="max-w-56 truncate p-2 font-mono text-xs">{e.target}</td>
                    <td class="p-2">{e.ok ? <Pill tone="emerald">ok</Pill> : <Pill tone="red">failed</Pill>}</td>
                  </tr>
                </Show>
              )}
            </For>
          </tbody>
        </table>
      </Card>
    </>
  );
}
