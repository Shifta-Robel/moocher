// A text box for prompts and answers. When a dictation engine is registered
// (src/voice.ts) it gets a microphone button: press to talk, press again to
// append the transcript.
import { JSX, Show, createSignal } from "solid-js";
import * as I from "../icons";
import { cx } from "../util";
import { dictation } from "../voice";
import { inputCls, notify } from "./ui";

export default function PromptBox(p: {
  value: string;
  onValue: (v: string) => void;
  rows?: number;
  placeholder?: string;
  label?: string;
  required?: boolean;
  class?: string;
  /** Ctrl/Cmd+Enter. */
  onSubmit?: () => void;
  /** A single line (Enter submits). */
  line?: boolean;
}) {
  const [rec, setRec] = createSignal<"idle" | "listening" | "transcribing">("idle");
  const mic = async () => {
    const d = dictation();
    if (!d) return;
    try {
      if (rec() === "idle") {
        await d.start();
        setRec("listening");
      } else if (rec() === "listening") {
        setRec("transcribing");
        const t = (await d.stop()).trim();
        if (t) p.onValue(p.value ? `${p.value.replace(/\s+$/, "")} ${t}` : t);
        setRec("idle");
      }
    } catch (e) {
      setRec("idle");
      notify("Dictation failed", e instanceof Error ? e.message : String(e), "error");
    }
  };
  const key: JSX.EventHandler<HTMLTextAreaElement | HTMLInputElement, KeyboardEvent> = (e) => {
    if (e.key === "Escape" && rec() === "listening") {
      dictation()?.cancel();
      setRec("idle");
    }
    if (e.key === "Enter" && p.onSubmit && (p.line || e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      p.onSubmit();
    }
  };
  return (
    <div class={cx("relative flex min-w-0", p.class)}>
      <Show
        when={p.line}
        fallback={
          <textarea class={cx(inputCls, dictation() && "pr-11")} rows={p.rows ?? 3} placeholder={p.placeholder} aria-label={p.label} required={p.required} value={p.value} onInput={(e) => p.onValue(e.currentTarget.value)} onKeyDown={key} />
        }
      >
        <input class={cx(inputCls, dictation() && "pr-11")} placeholder={p.placeholder} aria-label={p.label} required={p.required} value={p.value} onInput={(e) => p.onValue(e.currentTarget.value)} onKeyDown={key} />
      </Show>
      <Show when={dictation()}>
        <button
          type="button"
          class={cx(
            "absolute top-1.5 right-1.5 rounded-md p-1.5",
            rec() === "listening" ? "animate-pulse bg-red-600 text-white" : "text-slate-500 hover:bg-slate-100 dark:hover:bg-slate-800",
          )}
          onClick={mic}
          disabled={rec() === "transcribing"}
          aria-pressed={rec() === "listening"}
          aria-label={rec() === "listening" ? "stop dictation" : `dictate (${dictation()!.label})`}
          title={rec() === "listening" ? "stop and insert the text" : `dictate (${dictation()!.label})`}
        >
          <I.Mic class="size-4" />
        </button>
      </Show>
    </div>
  );
}
