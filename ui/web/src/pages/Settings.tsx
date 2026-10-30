import { For, Show, createResource, createSignal } from "solid-js";
import { auth, del, get, post } from "../api";
import { theme, setTheme } from "../components/Layout";
import { Button, Card, Field, PageHeader, Pill, act, inputCls } from "../components/ui";
import { state } from "../store";

export default function Settings() {
  const [old, setOld] = createSignal("");
  const [pw, setPw] = createSignal("");
  const [pw2, setPw2] = createSignal("");
  const admin = () => state.info?.scope === "admin";
  const [users, { refetch }] = createResource(admin, (a) => (a ? get<any[]>("/users").catch(() => []) : Promise.resolve([])));
  const [nu, setNu] = createSignal("");
  const [npw, setNpw] = createSignal("");
  const [nscope, setNscope] = createSignal("operate");
  return (
    <>
      <PageHeader title="Settings" subtitle={`moocher ${state.info?.version ?? ""} · logged in as ${state.info?.user ?? ""} (${state.info?.scope ?? ""})`} />
      <div class="grid gap-4 lg:grid-cols-2">
        <Card class="p-4">
          <h2 class="mb-3 font-semibold">Appearance</h2>
          <div class="flex gap-2" role="radiogroup" aria-label="theme">
            <For each={["system", "light", "dark"] as const}>
              {(t) => (
                <Button role="radio" aria-checked={theme() === t} variant={theme() === t ? "primary" : "default"} onClick={() => setTheme(t)}>
                  {t}
                </Button>
              )}
            </For>
          </div>
        </Card>
        <Card class="p-4">
          <h2 class="mb-3 font-semibold">This hub</h2>
          <div class="flex flex-wrap gap-2 text-sm">
            <Pill tone={state.info?.tls ? "emerald" : "amber"}>{state.info?.tls ? "HTTPS (built in)" : "no built-in TLS"}</Pill>
            <Pill tone={state.info?.attach ? "emerald" : "slate"}>Open in OpenCode {state.info?.attach ? "on" : "off"}</Pill>
            <Pill tone={state.info?.push.enabled ? "emerald" : "slate"}>Web Push {state.info?.push.enabled ? "on" : "off"}</Pill>
            <Pill tone={state.info?.ntfy ? "emerald" : "slate"}>ntfy {state.info?.ntfy ? "on" : "off"}</Pill>
            <Pill tone={state.info?.webhook ? "emerald" : "slate"}>webhook {state.info?.webhook ? "on" : "off"}</Pill>
          </div>
        </Card>
        <Card class="p-4">
          <h2 class="mb-3 font-semibold">Change password</h2>
          <form
            class="space-y-3"
            onSubmit={async (e) => {
              e.preventDefault();
              if (pw() !== pw2()) return act(async () => { throw new Error("the new passwords differ"); });
              if (await act(() => auth("password", { old: old(), new: pw() }), "Password changed")) {
                setOld(""); setPw(""); setPw2("");
              }
            }}
          >
            <Field label="Current password">
              <input class={inputCls} type="password" autocomplete="current-password" value={old()} onInput={(e) => setOld(e.currentTarget.value)} required />
            </Field>
            <Field label="New password" hint="at least 10 characters; other sessions are logged out">
              <input class={inputCls} type="password" autocomplete="new-password" minlength="10" value={pw()} onInput={(e) => setPw(e.currentTarget.value)} required />
            </Field>
            <Field label="Again">
              <input class={inputCls} type="password" autocomplete="new-password" value={pw2()} onInput={(e) => setPw2(e.currentTarget.value)} required />
            </Field>
            <Button type="submit" variant="primary">Change</Button>
          </form>
        </Card>
        <Show when={admin()}>
          <Card class="p-4">
            <h2 class="mb-3 font-semibold">Users</h2>
            <ul class="mb-3 divide-y divide-slate-100 text-sm dark:divide-slate-800">
              <For each={users() ?? []}>
                {(u) => (
                  <li class="flex items-center gap-2 py-2">
                    <span class="font-medium">{u.name}</span>
                    <Pill>{u.scope}</Pill>
                    <Show when={u.totp}>
                      <Pill tone="emerald">TOTP</Pill>
                    </Show>
                    <Show when={u.locked_until}>
                      <Pill tone="red">locked</Pill>
                    </Show>
                    <Show when={u.name !== state.info?.user}>
                      <Button size="sm" variant="ghost" class="ml-auto" onClick={async () => { if (confirm(`Remove ${u.name}?`) && (await act(() => del(`/users/${u.name}`), "Removed"))) refetch(); }}>
                        remove
                      </Button>
                    </Show>
                  </li>
                )}
              </For>
            </ul>
            <form class="grid gap-2 sm:grid-cols-4" onSubmit={async (e) => { e.preventDefault(); if (await act(() => post("/users", { name: nu(), password: npw(), scope: nscope() }), "User saved")) { setNu(""); setNpw(""); refetch(); } }}>
              <input class={inputCls} placeholder="name" pattern="[a-z0-9\-]+" value={nu()} onInput={(e) => setNu(e.currentTarget.value)} required />
              <input class={inputCls} type="password" placeholder="password" minlength="10" value={npw()} onInput={(e) => setNpw(e.currentTarget.value)} required />
              <select class={inputCls} value={nscope()} onChange={(e) => setNscope(e.currentTarget.value)}>
                <option value="read">read</option>
                <option value="operate">operate</option>
                <option value="admin">admin</option>
              </select>
              <Button type="submit">Add / set</Button>
            </form>
            <p class="mt-2 text-xs text-slate-500">TOTP is set up on the hub: moocher-hub user totp &lt;name&gt;.</p>
          </Card>
        </Show>
      </div>
    </>
  );
}
