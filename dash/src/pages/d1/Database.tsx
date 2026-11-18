import { Badge } from "@cloudflare/kumo/components/badge";
import { Banner } from "@cloudflare/kumo/components/banner";
import { Button } from "@cloudflare/kumo/components/button";
import { Code } from "@cloudflare/kumo/components/code";
import { InputArea } from "@cloudflare/kumo/components/input";
import { Table } from "@cloudflare/kumo/components/table";
import { DownloadSimpleIcon, PlayIcon, TrashIcon, UploadSimpleIcon } from "@phosphor-icons/react";
import { useQuery } from "@tanstack/react-query";
import { useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { useRef, useState } from "react";
import { useTranslation } from "react-i18next";

import { Crumbs } from "../../components/Crumbs";
import { ConfirmDelete } from "../../components/dialogs";
import { UrlTabs } from "../../components/TabbedPage";
import { Async, EmptyState, ErrorBanner, Facts, Mono, Page, Section, Time } from "../../components/ui";
import { accountPath, api, ApiError } from "../../lib/api";
import { errorText } from "../../lib/errors";
import { useFormat } from "../../lib/format";
import { useAccountApi, useAccountQuery, useChange } from "../../lib/hooks";
import { notifyDone, notifyFailed } from "../../lib/toast";
import type { D1Database, D1Raw } from "../../lib/types";

export function Database() {
  const { t } = useTranslation();
  const { database } = useParams({ strict: false }) as { database: string };
  const search = useSearch({ strict: false }) as { tab?: string };
  const { account, call } = useAccountApi();
  const navigate = useNavigate();
  const info = useAccountQuery<D1Database>(["d1", database], `/d1/database/${database}`);
  const [deleting, setDeleting] = useState(false);
  const remove = useChange(() => call(`/d1/database/${database}`, { method: "DELETE" }), { invalidate: [["d1"]], silentError: true });
  const name = info.data?.name ?? database;
  return (
    <Page
      crumbs={<Crumbs items={[{ label: t("nav.d1"), href: `/accounts/${account}/d1` }]} current={name} />}
      title={name}
      description={<Mono className="text-kumo-subtle">{database}</Mono>}
      actions={
        <Button variant="secondary-destructive" icon={TrashIcon} onClick={() => setDeleting(true)}>
          {t("common.delete")}
        </Button>
      }
    >
      <UrlTabs
        current={search.tab}
        tabs={[
          { value: "console", label: t("d1.tabs.console"), render: () => <Console database={database} /> },
          { value: "tables", label: t("d1.tabs.tables"), render: () => <Tables database={database} /> },
          { value: "backup", label: t("d1.tabs.backup"), render: () => <Backup database={database} info={info.data} /> },
        ]}
      />
      <ConfirmDelete
        open={deleting}
        onOpenChange={setDeleting}
        kind={t("kinds.database")}
        name={name}
        onConfirm={async () => {
          await remove.mutateAsync();
          navigate({ to: "/accounts/$account/d1", params: { account } });
        }}
      />
    </Page>
  );
}

/** Results of statements, as `raw` returns them: columns and rows. */
function ResultTable({ result }: { result: D1Raw }) {
  const { t } = useTranslation();
  const { columns, rows } = result.results;
  if (columns.length === 0) {
    return (
      <p className="p-4 text-kumo-subtle">
        {t("d1.changes", { count: result.meta?.changes ?? 0 })}
      </p>
    );
  }
  return (
    <div className="overflow-x-auto">
      <Table>
        <Table.Header>
          <Table.Row>
            {columns.map((c, i) => (
              <Table.Head key={i}>{c}</Table.Head>
            ))}
          </Table.Row>
        </Table.Header>
        <Table.Body>
          {rows.map((r, i) => (
            <Table.Row key={i}>
              {r.map((v, j) => (
                <Table.Cell key={j}>
                  {v === null ? <span className="text-kumo-subtle">NULL</span> : <Mono>{typeof v === "object" ? JSON.stringify(v) : String(v)}</Mono>}
                </Table.Cell>
              ))}
            </Table.Row>
          ))}
        </Table.Body>
      </Table>
    </div>
  );
}

function Console({ database }: { database: string }) {
  const { t } = useTranslation();
  const f = useFormat();
  const { account } = useAccountApi();
  const [sql, setSql] = useState("SELECT name FROM sqlite_schema WHERE type = 'table';");
  const [running, setRunning] = useState(false);
  const [results, setResults] = useState<D1Raw[]>();
  const [error, setError] = useState<unknown>();
  const run = async () => {
    setRunning(true);
    setError(undefined);
    try {
      setResults(await api<D1Raw[]>(accountPath(account, `/d1/database/${database}/raw`), { method: "POST", json: { sql } }));
    } catch (e) {
      setResults(undefined);
      setError(e);
    } finally {
      setRunning(false);
    }
  };
  return (
    <div className="flex flex-col gap-4">
      <Section title={t("d1.query")} description={t("d1.queryHelp")}>
        <form
          className="flex flex-col gap-3"
          onSubmit={(e) => {
            e.preventDefault();
            run();
          }}
        >
          <InputArea
            aria-label={t("d1.query")}
            value={sql}
            onValueChange={setSql}
            rows={6}
            spellCheck={false}
            className="font-mono"
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                e.preventDefault();
                run();
              }
            }}
          />
          <div className="flex items-center gap-3">
            <Button type="submit" variant="primary" icon={PlayIcon} loading={running} disabled={!sql.trim()}>
              {t("d1.run")}
            </Button>
            <span className="text-sm text-kumo-subtle">{t("d1.runShortcut")}</span>
          </div>
        </form>
      </Section>
      {error !== undefined && <ErrorBanner error={error} />}
      {results?.map((r, i) => (
        <Section
          key={i}
          title={t("d1.statement", { number: i + 1 })}
          actions={
            r.meta && (
              <span className="text-sm text-kumo-subtle">
                {t("d1.stats", {
                  rows: f.number(r.results.rows.length),
                  read: f.number(r.meta.rows_read ?? 0),
                  written: f.number(r.meta.rows_written ?? 0),
                  ms: f.number(Math.round((r.meta.duration ?? 0) * 100) / 100),
                })}
              </span>
            )
          }
          flush
        >
          <ResultTable result={r} />
        </Section>
      ))}
    </div>
  );
}

type SchemaRow = [string, string, string | null];

function Tables({ database }: { database: string }) {
  const { t } = useTranslation();
  const { account } = useAccountApi();
  const schema = useQuery({
    queryKey: [account, "d1-schema", database],
    queryFn: async () => {
      const r = await api<D1Raw[]>(accountPath(account, `/d1/database/${database}/raw`), {
        method: "POST",
        json: {
          sql: "SELECT type, name, sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' AND name NOT LIKE '_cf_%' ORDER BY type DESC, name",
        },
      });
      return r[0].results.rows as SchemaRow[];
    },
  });
  return (
    <Section title={t("d1.schema")} flush>
      <Async query={schema} empty={(s) => s.length === 0} emptyState={<EmptyState title={t("d1.noTables")} />}>
        {(rows) => (
          <Table>
            <Table.Header>
              <Table.Row>
                <Table.Head>{t("common.name")}</Table.Head>
                <Table.Head>{t("common.type")}</Table.Head>
                <Table.Head>{t("d1.definition")}</Table.Head>
              </Table.Row>
            </Table.Header>
            <Table.Body>
              {rows.map(([type, name, sql]) => (
                <Table.Row key={`${type}-${name}`}>
                  <Table.Cell>
                    <Mono>{name}</Mono>
                  </Table.Cell>
                  <Table.Cell>
                    <Badge variant="neutral">{type}</Badge>
                  </Table.Cell>
                  <Table.Cell>{sql ? <Code lang="ts" code={sql} /> : "—"}</Table.Cell>
                </Table.Row>
              ))}
            </Table.Body>
          </Table>
        )}
      </Async>
    </Section>
  );
}

function Backup({ database, info }: { database: string; info?: D1Database }) {
  const { t } = useTranslation();
  const f = useFormat();
  const { call } = useAccountApi();
  const [exporting, setExporting] = useState(false);
  const [importing, setImporting] = useState(false);
  const [importError, setImportError] = useState<string>();
  const file = useRef<HTMLInputElement>(null);

  const doExport = async () => {
    setExporting(true);
    try {
      const r = await call<{ status: string; result?: { signed_url: string; filename: string }; error?: string }>(
        `/d1/database/${database}/export`,
        { method: "POST", json: { output_format: "polling", dump_options: {} } },
      );
      if (r.status !== "complete" || !r.result) throw new ApiError(500, 0, r.error ?? "export failed");
      // Same origin: the download path is the dashboard's.
      const a = document.createElement("a");
      a.href = new URL(r.result.signed_url).pathname;
      a.download = r.result.filename;
      a.click();
    } catch (e) {
      notifyFailed(e, t);
    } finally {
      setExporting(false);
    }
  };

  const doImport = async (f: File) => {
    setImporting(true);
    setImportError(undefined);
    try {
      const body = await f.arrayBuffer();
      const { default: SparkMD5 } = await import("spark-md5");
      const etag = SparkMD5.ArrayBuffer.hash(body);
      const init = await call<{ upload_url: string; filename: string }>(`/d1/database/${database}/import`, {
        method: "POST",
        json: { action: "init", etag },
      });
      const up = await fetch(new URL(init.upload_url).pathname, { method: "PUT", body, credentials: "same-origin" });
      if (!up.ok) throw new ApiError(up.status, 0, up.statusText);
      const done = await call<{ status: string; error?: string; result?: { num_queries?: number } }>(
        `/d1/database/${database}/import`,
        { method: "POST", json: { action: "ingest", etag, filename: init.filename } },
      );
      if (done.status !== "complete") throw new ApiError(400, 0, done.error ?? "import failed");
      notifyDone(t("d1.imported", { count: done.result?.num_queries ?? 0 }));
    } catch (e) {
      setImportError(errorText(e, t));
    } finally {
      setImporting(false);
      if (file.current) file.current.value = "";
    }
  };

  return (
    <div className="flex flex-col gap-6">
      {info && (
        <Section title={t("d1.details")}>
          <Facts
            items={[
              [t("d1.size"), f.bytes(info.file_size)],
              [t("d1.tableCount"), f.number(info.num_tables)],
              [t("common.created"), <Time key="c" value={info.created_at} absolute />],
            ]}
          />
        </Section>
      )}
      <Section title={t("d1.export")} description={t("d1.exportHelp")}>
        <Button icon={DownloadSimpleIcon} loading={exporting} onClick={doExport}>
          {t("d1.exportButton")}
        </Button>
      </Section>
      <Section title={t("d1.import")} description={t("d1.importHelp")}>
        <div className="flex flex-col gap-3">
          {importError && <Banner variant="error" description={importError} />}
          <input
            ref={file}
            type="file"
            accept=".sql,text/plain"
            className="hidden"
            onChange={(e) => e.target.files?.[0] && doImport(e.target.files[0])}
          />
          <div>
            <Button icon={UploadSimpleIcon} loading={importing} onClick={() => file.current?.click()}>
              {t("d1.importButton")}
            </Button>
          </div>
        </div>
      </Section>
    </div>
  );
}
