// The dashboard talks to its own origin only: /auth/* for the session, and
// the API's /client/v4 handlers, which take the session cookie here. Every
// request carries X-Selvedge-Dash, which the server requires (RFC 0003 §4.4).

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: number,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

type Envelope<T> = {
  success: boolean;
  errors?: { code: number; message: string }[];
  result: T;
  result_info?: { cursor?: string; count?: number; page?: number; per_page?: number; total_count?: number };
};

export type RequestOptions = Omit<RequestInit, "body"> & {
  /** Sent as JSON. */
  json?: unknown;
  body?: BodyInit;
  query?: Record<string, string | number | boolean | undefined | null>;
};

let onUnauthorized: () => void = () => {};

/** Called when the session has ended (401), to send the user to login. */
export function setUnauthorizedHandler(f: () => void) {
  onUnauthorized = f;
}

function withQuery(path: string, query?: RequestOptions["query"]) {
  if (!query) return path;
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) {
    if (v !== undefined && v !== null && v !== "") q.set(k, String(v));
  }
  const s = q.toString();
  return s ? `${path}${path.includes("?") ? "&" : "?"}${s}` : path;
}

async function send(path: string, opts: RequestOptions = {}): Promise<Response> {
  const { json, query, body, ...init } = opts;
  const headers = new Headers(init.headers);
  headers.set("x-selvedge-dash", "1");
  let payload = body;
  if (json !== undefined) {
    headers.set("content-type", "application/json");
    payload = JSON.stringify(json);
  }
  const res = await fetch(withQuery(path, query), { ...init, headers, body: payload, credentials: "same-origin" });
  if (res.status === 401 && !path.startsWith("/auth/")) onUnauthorized();
  return res;
}

async function fail(res: Response): Promise<never> {
  let code = 0;
  let message = res.statusText;
  try {
    const env = (await res.json()) as Envelope<unknown>;
    if (env.errors?.[0]) ({ code, message } = env.errors[0]);
  } catch {
    // Not JSON: keep the status text.
  }
  throw new ApiError(res.status, code, message);
}

/** A JSON API call: the envelope's `result`. */
export async function api<T>(path: string, opts?: RequestOptions): Promise<T> {
  return (await apiPage<T>(path, opts)).result;
}

/** A JSON API call with the envelope's paging information. */
export async function apiPage<T>(path: string, opts?: RequestOptions): Promise<Envelope<T>> {
  const res = await send(path, opts);
  if (!res.ok) await fail(res);
  const env = (await res.json()) as Envelope<T>;
  if (env.success === false) {
    const e = env.errors?.[0];
    throw new ApiError(res.status, e?.code ?? 0, e?.message ?? "request failed");
  }
  return env;
}

/** A call whose answer is not JSON (a KV value, an R2 object). */
export async function apiRaw(path: string, opts?: RequestOptions): Promise<Response> {
  const res = await send(path, opts);
  if (!res.ok) await fail(res);
  return res;
}

export const accountPath = (account: string, path = "") =>
  `/client/v4/accounts/${encodeURIComponent(account)}${path}`;

/** Encodes each segment of a key that may contain slashes. */
export const encodeKey = (key: string) => key.split("/").map(encodeURIComponent).join("/");
