// Trace events from a tail session, and what the log view filters on.

/** A trace event as `wrangler tail` receives it. */
export type TailEvent = {
  outcome: string;
  eventTimestamp: number;
  event?: {
    request?: { url: string; method: string };
    response?: { status: number };
    cron?: string;
    rpcMethod?: string;
  } | null;
  logs: { message: unknown[]; level: string; timestamp: number }[];
  exceptions: { name: string; message: string; timestamp: number }[];
  truncated?: boolean;
};


/** What a filter matches against: the event's summary, logs and exceptions. */
export function eventText(e: TailEvent): string {
  const parts: string[] = [];
  if (e.event?.request) parts.push(e.event.request.method, e.event.request.url);
  for (const l of e.logs) parts.push(l.message.map((m) => (typeof m === "string" ? m : JSON.stringify(m))).join(" "));
  for (const x of e.exceptions) parts.push(x.name, x.message);
  return parts.join(" ").toLowerCase();
}

export function isError(e: TailEvent): boolean {
  return e.outcome !== "ok" || e.exceptions.length > 0 || (e.event?.response?.status ?? 0) >= 500;
}

