// Mirrors of the supervisor's JSON (shared-protocol/src/{model,api}.rs), the parts the UI uses.
export type State =
  | "starting" | "idle" | "running" | "blocked" | "parked" | "error"
  | "stopped" | "unreachable" | "lost" | "dead" | "halted";

export interface Todo { content: string; status: string; priority?: string }
export interface Tokens { input: number; output: number; reasoning: number; cache_read: number; cache_write: number; cost_usd: number }

export interface Session {
  id: string; node: string; agent: string; state: State;
  tokens: Tokens; todos: Todo[]; current_task?: string | null; last_text?: string | null; error?: string | null;
  model?: string | null; workspace?: string | null; started_at: number; updated_at: number;
  context_tokens?: number; dead_reason?: string | null; turns?: number;
  title?: string | null; task_id?: number | null; parent?: string | null; run_id?: string | null; step?: string | null;
  verdict?: string | null; role?: string; placement?: string | null; waiting_on?: string[]; deaths?: number;
  replaces?: string | null; prompt?: string | null; attached_by?: string | null; attached_until?: number | null;
}

export interface AgentInfo {
  name: string; description: string; provider: string; model: string; tier: string; secret_ok: boolean;
  errors: string[]; routes: string[]; source: string; max_instances?: number | null;
  overridable: { role_append: boolean; budget_usd?: number | null; routes: string[] };
  price_in_per_mtok?: number;
}

export interface NodeInfo {
  name: string; connected: boolean; version: string; max_sessions: number; running: number; relay: boolean;
  agents: AgentInfo[]; last_seen: number; labels: Record<string, string>; role: string; enrollment: string;
  observed_ip?: string | null; cert_expires_at?: number | null; link_rtt_ms?: number | null;
}

export interface Task {
  id: number; title: string; body: string; label: "urgent" | "when_free" | "on_demand"; rank: number;
  status: "queued" | "running" | "done" | "failed" | "cancelled" | "halted"; tags: string[];
  coordinator_session?: string | null; result?: string | null; notes: string[]; created_at: number;
  started_at?: number | null; finished_at?: number | null; question_mode?: string | null;
}

export interface Overview { nodes: NodeInfo[]; sessions: Session[]; tasks: Task[]; totals: Tokens }

export interface Hop { holder: string; at: number; why: string; note?: string | null }
export interface Question {
  id: string; kind: "ask" | "opencode" | "permission" | "budget"; asker: string; asker_agent: string; node: string;
  task_id?: number | null; run_id?: string | null; text: string; options: { label: string; description: string }[];
  context?: string | null; importance: "low" | "normal" | "vital"; raised_because: string[]; operator_only: boolean;
  mode: string; holder: string; top_decides: boolean; chain: Hop[]; status: string; answer?: string | null;
  answered_by?: string | null; blocked: number; created_at: number; updated_at: number;
}

export interface RecoveryOption { kind: string; available: boolean; est_cost_usd: number; est_requests?: number | null; note: string }
export interface Halt {
  id: string; scope: string; dead_session: string; reason: string; at: number; spent_usd: number;
  current_todo?: string | null; options: RecoveryOption[]; sessions: string[]; status: string; resolution?: string | null;
}

export interface Notification { id: number; ts: number; kind: string; severity: "info" | "warning" | "vital"; title: string; body: string; target?: string | null; read: boolean }

export interface Quota {
  provider: string; scope: string; scope_id: string; kind: string; limit?: number | null; used: number;
  remaining?: number | null; resets_at?: number | null; exhausted: boolean; source: string; window?: string | null;
  nodes: string[]; routes: string[]; sessions: string[]; updated_at: number;
}

export interface Artifact {
  reference: string; hash: string; size: number; name: string; media_type: string; kind: "file" | "tree";
  producer?: string | null; node?: string | null; parents: string[]; tags: string[]; task_id?: number | null; created_at: number;
}

export interface LogEvent { id: number; ts: number; kind: string; node?: string | null; session?: string | null; data: any }

export interface WorkflowRun {
  id: string; workflow: string; status: string; input: string; created_at: number; updated_at: number; task_id?: number | null;
  steps: { step: string; session: string; verdict?: string | null; nudges: number; output?: string | null; started_at: number; finished_at?: number | null; check_results: string[] }[];
}

export interface Info { version: string; user: string; scope: string; attach: boolean; tls: boolean; push: { enabled: boolean; public_key: string }; question_mode: string; ntfy: boolean; webhook: boolean }
