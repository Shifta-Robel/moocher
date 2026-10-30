// Dictation: speech in, text out, appended to a prompt box. No engine ships
// yet. One may run on the device (the browser's own recognizer, an app's),
// on the hub, or at a provider through the hub; each only has to turn a
// recording into text, so they all fit this one shape. Register one with
// `registerDictation` and every PromptBox shows its microphone button.
import { createSignal } from "solid-js";

export interface Dictation {
  id: string;
  label: string;
  /** Start capturing. Rejects if the microphone is unavailable or refused. */
  start(): Promise<void>;
  /** Stop capturing and return the transcript (may be empty). */
  stop(): Promise<string>;
  /** Stop capturing and discard. */
  cancel(): void;
}

const [engines, setEngines] = createSignal<Dictation[]>([]);

export function registerDictation(d: Dictation) {
  setEngines((es) => [...es.filter((e) => e.id !== d.id), d]);
}

/** The engine prompt boxes use, if any. */
export const dictation = () => engines()[0];
