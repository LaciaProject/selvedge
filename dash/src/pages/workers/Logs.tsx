import { Badge } from "@cloudflare/kumo/components/badge";
import { Button } from "@cloudflare/kumo/components/button";
import { Input } from "@cloudflare/kumo/components/input";
import { Select } from "@cloudflare/kumo/components/select";
import { PauseIcon, PlayIcon, TrashIcon } from "@phosphor-icons/react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

import { EmptyState, Section } from "../../components/ui";
import { errorText } from "../../lib/errors";
import { useFormat } from "../../lib/format";
import { useAccountApi } from "../../lib/hooks";
import { eventText, isError, type TailEvent } from "../../lib/tail";
import type { TailSession } from "../../lib/types";

const KEEP = 1000;
const METHODS = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
type Status = "all" | "ok" | "error";

export function Logs({ script }: { script: string }) {
  const { t } = useTranslation();
  const f = useFormat();
  const { call } = useAccountApi();
  const [running, setRunning] = useState(false);
  const [state, setState] = useState<"idle" | "connecting" | "live" | "error">("idle");
  const [problem, setProblem] = useState<string>();
  const [events, setEvents] = useState<TailEvent[]>([]);
  const [status, setStatus] = useState<Status>("all");
  const [method, setMethod] = useState("all");
  const [text, setText] = useState("");
  const socket = useRef<WebSocket | null>(null);
  const wanted = useRef(false);
  // For connecting again from the socket's close handler.
  const reconnect = useRef<() => void>(() => {});

  const connect = useCallback(async () => {
    setState("connecting");
    setProblem(undefined);
    try {
      const session = await call<TailSession>(`/workers/scripts/${encodeURIComponent(script)}/tails`, {
        method: "POST",
        json: { filters: [] },
      });
      if (!wanted.current) return;
      const ws = new WebSocket(session.url, "trace-v1");
      socket.current = ws;
      ws.onopen = () => setState("live");
      ws.onmessage = async (m) => {
        const raw = typeof m.data === "string" ? m.data : await (m.data as Blob).text();
        try {
          const e = JSON.parse(raw) as TailEvent;
          setEvents((prev) => [e, ...prev].slice(0, KEEP));
        } catch {
          // Not an event.
        }
      };
      ws.onclose = () => {
        socket.current = null;
        // The session expired or the connection dropped: start another.
        if (wanted.current) {
          setState("connecting");
          setTimeout(() => wanted.current && reconnect.current(), 2000);
        } else {
          setState("idle");
        }
      };
    } catch (e) {
      setState("error");
      setProblem(errorText(e, t));
      wanted.current = false;
      setRunning(false);
    }
  }, [call, script, t]);

  useEffect(() => {
    reconnect.current = connect;
  }, [connect]);

  const toggle = () => {
    wanted.current = !running;
    setRunning(!running);
    if (wanted.current) connect();
    else socket.current?.close();
  };

  useEffect(
    () => () => {
      wanted.current = false;
      socket.current?.close();
    },
    [],
  );

  const needle = text.trim().toLowerCase();
  const shown = events.filter(
    (e) =>
      (status === "all" || (status === "error") === isError(e)) &&
      (method === "all" || e.event?.request?.method === method) &&
      (!needle || eventText(e).includes(needle)),
  );

  return (
    <Section
      title={t("logs.title")}
      description={t("logs.help")}
      actions={
        <>
          <Badge variant={state === "live" ? "success" : state === "error" ? "error" : "neutral"} appearance="dot">
            {t(`logs.state.${state}`)}
          </Badge>
          <Button
            size="sm"
            variant={running ? "secondary" : "primary"}
            icon={running ? PauseIcon : PlayIcon}
            onClick={toggle}
          >
            {running ? t("logs.stop") : t("logs.start")}
          </Button>
          <Button size="sm" variant="ghost" icon={TrashIcon} onClick={() => setEvents([])} disabled={events.length === 0}>
            {t("logs.clear")}
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-4">
        <div className="flex flex-wrap gap-2">
          <Select
            aria-label={t("logs.status")}
            value={status}
            onValueChange={(v) => setStatus(v as Status)}
            items={{ all: t("logs.all"), ok: t("logs.ok"), error: t("logs.errors") }}
          >
            <Select.Option value="all">{t("logs.all")}</Select.Option>
            <Select.Option value="ok">{t("logs.ok")}</Select.Option>
            <Select.Option value="error">{t("logs.errors")}</Select.Option>
          </Select>
          <Select
            aria-label={t("logs.method")}
            value={method}
            onValueChange={(v) => setMethod(String(v))}
            items={{ all: t("logs.allMethods"), ...Object.fromEntries(METHODS.map((m) => [m, m])) }}
          >
            <Select.Option value="all">{t("logs.allMethods")}</Select.Option>
            {METHODS.map((m) => (
              <Select.Option key={m} value={m}>
                {m}
              </Select.Option>
            ))}
          </Select>
          <Input
            aria-label={t("logs.search")}
            placeholder={t("logs.search")}
            value={text}
            onChange={(e) => setText(e.target.value)}
            className="min-w-48 flex-1"
          />
        </div>
        {problem && <p className="text-kumo-danger">{problem}</p>}
        {shown.length === 0 ? (
          <EmptyState title={running ? t("logs.waiting") : t("logs.notRunning")} description={running ? t("logs.waitingHelp") : undefined} />
        ) : (
          <ol className="flex flex-col divide-y divide-kumo-hairline font-mono text-sm" aria-label={t("logs.title")}>
            {shown.map((e, i) => (
              <li key={`${e.eventTimestamp}-${i}`} className="flex flex-col gap-1 py-2" data-testid="log-event">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-kumo-subtle" title={f.utc(e.eventTimestamp)}>
                    {f.time(e.eventTimestamp)}
                  </span>
                  <Badge variant={isError(e) ? "error" : "success"}>{e.outcome}</Badge>
                  <span className="break-all">{summary(e)}</span>
                </div>
                {e.logs.map((l, j) => (
                  <div key={j} className={l.level === "error" ? "pl-4 text-kumo-danger" : "pl-4 text-kumo-strong"}>
                    {l.message.map((m) => (typeof m === "string" ? m : JSON.stringify(m))).join(" ")}
                  </div>
                ))}
                {e.exceptions.map((x, j) => (
                  <div key={`x${j}`} className="pl-4 text-kumo-danger">
                    {x.name}: {x.message}
                  </div>
                ))}
              </li>
            ))}
          </ol>
        )}
      </div>
    </Section>
  );
}

function summary(e: TailEvent): string {
  const ev = e.event;
  if (ev?.request) {
    let path = ev.request.url;
    try {
      const u = new URL(ev.request.url);
      path = u.pathname + u.search;
    } catch {
      // Keep the URL as given.
    }
    return `${ev.request.method} ${path}${ev.response ? ` ${ev.response.status}` : ""}`;
  }
  if (ev?.cron) return `cron ${ev.cron}`;
  if (ev?.rpcMethod) return `rpc ${ev.rpcMethod}`;
  return "";
}
