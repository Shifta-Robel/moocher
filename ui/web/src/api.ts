// Fetch wrapper: same-origin cookie session + CSRF header on changes.
export class ApiError extends Error {
  constructor(public status: number, message: string, public body?: any) {
    super(message);
  }
}

export function csrf(): string {
  const c = document.cookie.split(";").map((s) => s.trim()).find((s) => s.startsWith("ocl_csrf="));
  return c ? decodeURIComponent(c.slice(9)) : "";
}

async function request<T>(method: string, url: string, body?: unknown, raw?: BodyInit): Promise<T> {
  const headers: Record<string, string> = { "x-csrf-token": csrf() };
  if (body !== undefined) headers["content-type"] = "application/json";
  const r = await fetch(url, { method, headers, body: raw ?? (body === undefined ? undefined : JSON.stringify(body)), credentials: "same-origin" });
  const text = await r.text();
  let data: any = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = text; }
  if (r.status === 401 && url.startsWith("/api/")) {
    const next = location.pathname === "/login" ? "" : `?next=${encodeURIComponent(location.pathname)}`;
    location.assign(`/login${next}`);
  }
  if (!r.ok) throw new ApiError(r.status, (data && data.error) || `${r.status} ${r.statusText}`, data);
  return data as T;
}

const v1 = (p: string) => `/api/v1${p}`;
export const get = <T>(p: string) => request<T>("GET", v1(p));
export const post = <T>(p: string, body: unknown = {}) => request<T>("POST", v1(p), body);
export const patch = <T>(p: string, body: unknown) => request<T>("PATCH", v1(p), body);
export const del = <T>(p: string) => request<T>("DELETE", v1(p));
export const upload = <T>(p: string, file: Blob) => request<T>("POST", v1(p), undefined, file);
export const auth = <T>(p: string, body?: unknown) => request<T>(body === undefined ? "GET" : "POST", `/auth/${p}`, body);
