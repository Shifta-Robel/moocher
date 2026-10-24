// The cluster as the UI sees it: loaded once, then kept current by the SSE stream.
import { createStore, produce, reconcile } from "solid-js/store";
import { createSignal } from "solid-js";
import { get } from "./api";
import type { Halt, Info, LogEvent, NodeInfo, Notification, Overview, Question, Quota, Session, Task, WorkflowRun } from "./types";

export const [state, setState] = createStore({
  ready: false,
  connected: false,
  info: null as Info | null,
  nodes: [] as NodeInfo[],
  sessions: [] as Session[],
  tasks: [] as Task[],
  questions: [] as Question[], // open ones
  halts: [] as Halt[], // open ones
  notifications: [] as Notification[],
  unread: 0,
  quotas: [] as Quota[],
  runs: [] as WorkflowRun[],
  totals: { input: 0, output: 0, reasoning: 0, cache_read: 0, cache_write: 0, cost_usd: 0 },
});

/** Recent log events, newest last (bounded). */
export const [events, setEvents] = createSignal<LogEvent[]>([]);
/** Bumped when a node reports new resource samples. */
export const [statsTick, setStatsTick] = createSignal(0);

function upsert<T>(key: keyof typeof state, item: T, id: (x: T) => string | number) {
  setState(
    produce((s: any) => {
      const list: T[] = s[key];
      const i = list.findIndex((x) => id(x) === id(item));
      if (i >= 0) list[i] = item;
      else list.unshift(item);
    }),
  );
}

export async function refreshQuotas() {
  setState("quotas", reconcile(await get<Quota[]>("/quotas")));
}

export async function refreshAll() {
  const [ov, qs, hs, ns, info, runs] = await Promise.all([
    get<Overview>("/overview"),
    get<Question[]>("/questions"),
    get<Halt[]>("/halts"),
    get<Notification[]>("/notifications?limit=100"),
    get<Info>("/info"),
    get<WorkflowRun[]>("/runs"),
  ]);
  setState({
    nodes: ov.nodes,
    sessions: ov.sessions,
    tasks: ov.tasks,
    totals: ov.totals,
    questions: qs,
    halts: hs,
    notifications: ns,
    unread: ns.filter((n) => !n.read).length,
    info,
    runs,
    ready: true,
  });
  refreshQuotas().catch(() => {});
}

let es: EventSource | null = null;
let quotaTimer: number | undefined;

export function connect() {
  es?.close();
  es = new EventSource("/api/v1/stream");
  es.onopen = () => {
    setState("connected", true);
    // Anything missed while disconnected.
    refreshAll().catch(() => {});
  };
  es.onerror = () => setState("connected", false);
  es.onmessage = (m) => {
    let ev: any;
    try { ev = JSON.parse(m.data); } catch { return; }
    switch (ev.type) {
      case "session":
        upsert<Session>("sessions", ev as Session, (s) => s.id);
        break;
      case "node":
        upsert<NodeInfo>("nodes", ev as NodeInfo, (n) => n.name);
        break;
      case "task":
        upsert<Task>("tasks", ev as Task, (t) => t.id);
        break;
      case "run":
        upsert<WorkflowRun>("runs", ev as WorkflowRun, (r) => r.id);
        break;
      case "question": {
        const q = ev as Question;
        if (q.status === "open") upsert<Question>("questions", q, (x) => x.id);
        else setState("questions", (l) => l.filter((x) => x.id !== q.id));
        break;
      }
      case "halt": {
        const h = ev as Halt;
        if (h.status === "open") upsert<Halt>("halts", h, (x) => x.id);
        else setState("halts", (l) => l.filter((x) => x.id !== h.id));
        break;
      }
      case "notification":
        setState(produce((s) => { s.notifications.unshift(ev as Notification); s.notifications.splice(200); s.unread += 1; }));
        break;
      case "quotas":
        clearTimeout(quotaTimer);
        quotaTimer = window.setTimeout(() => refreshQuotas().catch(() => {}), 500);
        break;
      case "stats":
        setStatsTick((n) => n + 1);
        break;
      case "log":
        setEvents((l) => (l.length > 500 ? [...l.slice(-400), ev as LogEvent] : [...l, ev as LogEvent]));
        break;
    }
  };
}

export const liveStates = ["starting", "idle", "running", "blocked", "parked"];
export const isLive = (s: Session) => liveStates.includes(s.state);
