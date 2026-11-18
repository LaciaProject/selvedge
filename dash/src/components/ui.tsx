import { Banner } from "@cloudflare/kumo/components/banner";
import { Button } from "@cloudflare/kumo/components/button";
import { Empty } from "@cloudflare/kumo/components/empty";
import { LayerCard } from "@cloudflare/kumo/components/layer-card";
import { Loader } from "@cloudflare/kumo/components/loader";
import { Text } from "@cloudflare/kumo/components/text";
import { cn } from "@cloudflare/kumo/utils";
import { WarningCircleIcon } from "@phosphor-icons/react";
import type { UseQueryResult } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { useTranslation } from "react-i18next";

import { describeError } from "../lib/errors";
import { useFormat } from "../lib/format";

/** A page: its title, what it is for, and its main actions. */
export function Page(props: {
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  crumbs?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="mx-auto flex w-full max-w-6xl flex-col gap-6 px-4 py-6 sm:px-8">
      {props.crumbs}
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div className="flex min-w-0 flex-col gap-1">
          <Text variant="heading" size="lg" as="h1">
            {props.title}
          </Text>
          {props.description && <Text variant="secondary">{props.description}</Text>}
        </div>
        {props.actions && <div className="flex flex-wrap items-center gap-2">{props.actions}</div>}
      </div>
      {props.children}
    </div>
  );
}

/** A titled card within a page. */
export function Section(props: {
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
  flush?: boolean;
}) {
  return (
    <LayerCard>
      <LayerCard.Secondary className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-col">
          <span className="font-medium">{props.title}</span>
          {props.description && <Text variant="secondary" size="sm">{props.description}</Text>}
        </div>
        {props.actions && <div className="flex items-center gap-2">{props.actions}</div>}
      </LayerCard.Secondary>
      <LayerCard.Primary className={cn(props.flush && "p-0 overflow-x-auto")}>{props.children}</LayerCard.Primary>
    </LayerCard>
  );
}

export function Loading({ className }: { className?: string }) {
  const { t } = useTranslation();
  return (
    <div className={cn("flex items-center justify-center gap-2 p-8 text-kumo-subtle", className)} role="status">
      <Loader size="sm" />
      <span>{t("common.loading")}</span>
    </div>
  );
}

export function ErrorBanner({ error, onRetry }: { error: unknown; onRetry?: () => void }) {
  const { t } = useTranslation();
  const { title, detail } = describeError(error, t);
  return (
    <Banner
      variant="error"
      icon={<WarningCircleIcon weight="fill" />}
      title={title}
      description={detail}
      action={
        onRetry && (
          <Button size="sm" variant="secondary" onClick={onRetry}>
            {t("common.retry")}
          </Button>
        )
      }
    />
  );
}

/** Loading, error, empty or the data. */
export function Async<T>(props: {
  query: UseQueryResult<T>;
  empty?: (data: T) => boolean;
  emptyState?: ReactNode;
  children: (data: T) => ReactNode;
}) {
  const q = props.query;
  if (q.isPending) return <Loading />;
  if (q.isError) return <ErrorBanner error={q.error} onRetry={() => q.refetch()} />;
  if (props.empty?.(q.data)) return <>{props.emptyState}</>;
  return <>{props.children(q.data)}</>;
}

export function EmptyState(props: { icon?: ReactNode; title: string; description?: string; action?: ReactNode }) {
  return (
    <Empty size="sm" icon={props.icon} title={props.title} description={props.description} contents={props.action} />
  );
}

/** A time: relative, with the exact time on hover. */
export function Time({ value, absolute }: { value?: string | number | null; absolute?: boolean }) {
  const f = useFormat();
  if (value === undefined || value === null || value === "") return <span className="text-kumo-subtle">—</span>;
  const d = new Date(value);
  if (Number.isNaN(d.getTime())) return <span>{String(value)}</span>;
  return (
    <time dateTime={d.toISOString()} title={`${f.date(d)} (${f.utc(d)})`}>
      {absolute ? f.date(d) : f.relative(d)}
    </time>
  );
}

export function Mono({ children, className }: { children: ReactNode; className?: string }) {
  return <span className={cn("font-mono text-sm break-all", className)}>{children}</span>;
}

/** Label/value pairs. */
export function Facts({ items }: { items: [ReactNode, ReactNode][] }) {
  return (
    <dl className="grid grid-cols-1 gap-x-6 gap-y-3 sm:grid-cols-[max-content_1fr]">
      {items.map(([label, value], i) => (
        <div key={i} className="contents">
          <dt className="text-kumo-subtle">{label}</dt>
          <dd className="min-w-0 break-words">{value}</dd>
        </div>
      ))}
    </dl>
  );
}
