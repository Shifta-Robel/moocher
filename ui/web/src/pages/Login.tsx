import { createSignal, Show } from "solid-js";
import { ApiError, auth } from "../api";
import { Button, Field, inputCls } from "../components/ui";

export default function Login() {
  const [user, setUser] = createSignal("");
  const [pw, setPw] = createSignal("");
  const [code, setCode] = createSignal("");
  const [needCode, setNeedCode] = createSignal(false);
  const [err, setErr] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const next = () => {
    const n = new URLSearchParams(location.search).get("next") || "/";
    return n.startsWith("/") && !n.startsWith("//") ? n : "/";
  };
  const submit = async (e: Event) => {
    e.preventDefault();
    setBusy(true);
    setErr("");
    try {
      await auth("login", { user: user(), password: pw(), totp: needCode() ? code() : undefined });
      location.assign(next());
    } catch (x) {
      const e = x as ApiError;
      if (e.body?.totp_required) {
        setNeedCode(true);
        setErr(needCode() ? e.message : "");
      } else setErr(e.message);
    } finally {
      setBusy(false);
    }
  };
  return (
    <div class="flex min-h-dvh items-center justify-center p-4">
      <form onSubmit={submit} class="w-full max-w-sm space-y-4 rounded-2xl bg-white p-6 shadow-sm ring-1 ring-slate-200 dark:bg-slate-900 dark:ring-slate-800">
        <div class="flex items-center gap-2">
          <img src="/icon.svg" class="size-8" alt="" />
          <h1 class="text-lg font-semibold">moocher</h1>
        </div>
        <Field label="User">
          <input class={inputCls} autocomplete="username" value={user()} onInput={(e) => setUser(e.currentTarget.value)} required autofocus />
        </Field>
        <Field label="Password">
          <input class={inputCls} type="password" autocomplete="current-password" value={pw()} onInput={(e) => setPw(e.currentTarget.value)} required />
        </Field>
        <Show when={needCode()}>
          <Field label="Code from your authenticator app">
            <input class={inputCls} inputmode="numeric" autocomplete="one-time-code" pattern="[0-9]{6}" maxlength="6" value={code()} onInput={(e) => setCode(e.currentTarget.value)} required autofocus />
          </Field>
        </Show>
        <Show when={err()}>
          <p class="rounded-lg bg-red-50 p-2 text-sm text-red-800 dark:bg-red-950 dark:text-red-300" role="alert">
            {err()}
          </p>
        </Show>
        <Button type="submit" variant="primary" class="w-full" busy={busy()}>
          Log in
        </Button>
      </form>
    </div>
  );
}
