import { A } from "@solidjs/router";
import { For, Show, createResource, onMount } from "solid-js";
import { post } from "../api";
import { Button, Card, Empty, PageHeader, act } from "../components/ui";
import * as I from "../icons";
import { disablePush, enablePush, pushState } from "../push";
import { setState, state } from "../store";
import { ago, cx } from "../util";

const link = (t?: string | null) => (!t ? null : t.startsWith("q-") ? "/questions" : t.startsWith("h-") || t.startsWith("task:") ? "/tasks" : t.startsWith("s-") ? `/instances/${t}` : t.startsWith("node:") ? "/nodes" : null);

export default function Notifications() {
  const [push, { refetch }] = createResource(pushState);
  onMount(() => {
    const max = Math.max(0, ...state.notifications.map((n) => n.id));
    if (max && state.unread) {
      post("/notifications/read", { upto: max }).then(() => {
        setState("unread", 0);
        setState("notifications", (l) => l.map((n) => ({ ...n, read: true })));
      });
    }
  });
  return (
    <>
      <PageHeader
        title="Notifications"
        subtitle="Everything goes here; rules in supervisor.yaml decide what also goes to push, ntfy or a webhook."
        actions={
          <>
            <Show when={push() === "off"}>
              <Button variant="primary" onClick={async () => { await act(enablePush, "Push notifications on for this browser"); refetch(); }}>
                <I.Bell class="size-4" /> Enable push on this device
              </Button>
            </Show>
            <Show when={push() === "on"}>
              <Button onClick={async () => { await act(disablePush, "Push off for this browser"); refetch(); }}>Disable push here</Button>
            </Show>
            <Button variant="ghost" onClick={() => act(() => post("/notifications/test"), "Test sent")}>Send a test</Button>
          </>
        }
      />
      <Show when={push() === "denied"}>
        <p class="mb-3 text-sm text-amber-700 dark:text-amber-400">This browser blocks notifications for the dashboard; allow them in the site settings to enable push.</p>
      </Show>
      <Show when={push() === "unsupported"}>
        <p class="mb-3 text-sm text-slate-500">Push needs HTTPS and a browser with service workers (on iPhone: add the dashboard to the home screen first).</p>
      </Show>
      <Show when={state.notifications.length} fallback={<Empty icon={I.Bell} title="Nothing yet" />}>
        <Card class="divide-y divide-slate-100 dark:divide-slate-800">
          <For each={state.notifications}>
            {(n) => (
              <div class={cx("flex gap-3 p-3", !n.read && "bg-indigo-50/60 dark:bg-indigo-950/30")}>
                <Show when={n.severity === "vital"} fallback={n.severity === "warning" ? <I.AlertTriangle class="mt-0.5 size-5 shrink-0 text-amber-500" /> : <I.Bell class="mt-0.5 size-5 shrink-0 text-slate-400" />}>
                  <I.AlertTriangle class="mt-0.5 size-5 shrink-0 text-red-600" />
                </Show>
                <div class="min-w-0 flex-1">
                  <div class="flex flex-wrap items-baseline gap-2">
                    <Show when={link(n.target)} fallback={<span class="font-medium">{n.title}</span>}>
                      <A href={link(n.target)!} class="font-medium hover:underline">{n.title}</A>
                    </Show>
                    <span class="text-xs text-slate-500">
                      {n.severity} · {n.kind} · {ago(n.ts)} ago
                    </span>
                  </div>
                  <Show when={n.body}>
                    <p class="text-sm break-words text-slate-600 dark:text-slate-400">{n.body}</p>
                  </Show>
                </div>
              </div>
            )}
          </For>
        </Card>
      </Show>
    </>
  );
}
