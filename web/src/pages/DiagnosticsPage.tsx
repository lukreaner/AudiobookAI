import { useMutation, useQuery } from "@tanstack/react-query";
import { Bug, Download, RefreshCw, Search, ShieldCheck } from "lucide-react";
import { FormEvent, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "../api/client";
import type { DiagnosticEntry, DiagnosticLevel, DiagnosticQuery } from "../api/types";
import { EmptyState, ErrorState, LoadingState } from "../components/StateViews";
import { Badge, Button, Card, Field, Input, PageHeading, Select } from "../components/ui";

type LevelFilter = DiagnosticLevel | "all";

const levelTone: Record<DiagnosticLevel, "neutral" | "info" | "positive" | "warning" | "danger"> = {
  trace: "neutral",
  debug: "info",
  info: "positive",
  warn: "warning",
  error: "danger",
};

export function DiagnosticsPage() {
  const { t, i18n } = useTranslation();
  const [level, setLevel] = useState<LevelFilter>("info");
  const [target, setTarget] = useState("");
  const [searchDraft, setSearchDraft] = useState("");
  const [search, setSearch] = useState("");
  const [autoRefresh, setAutoRefresh] = useState(true);
  const query = useMemo<DiagnosticQuery>(() => ({
    level: level === "all" ? undefined : level,
    target: target.trim() || undefined,
    search: search.trim() || undefined,
    limit: 500,
  }), [level, search, target]);
  const diagnostics = useQuery({
    queryKey: ["diagnostics", query],
    queryFn: () => api.diagnostics(query),
    refetchInterval: autoRefresh ? 3_000 : false,
  });
  const download = useMutation({ mutationFn: () => api.downloadDiagnostics(query) });

  const applySearch = (event: FormEvent) => {
    event.preventDefault();
    setSearch(searchDraft);
  };

  return (
    <>
      <PageHeading
        eyebrow={t("diagnostics.eyebrow")}
        title={t("diagnostics.title")}
        subtitle={t("diagnostics.subtitle")}
        actions={(
          <div className="cluster">
            <Button variant="secondary" onClick={() => diagnostics.refetch()} disabled={diagnostics.isFetching}>
              <RefreshCw className={diagnostics.isFetching ? "spin" : undefined} size={16} />
              {t("common.refresh")}
            </Button>
            <Button onClick={() => download.mutate()} disabled={download.isPending}>
              <Download size={16} />
              {t("diagnostics.export")}
            </Button>
          </div>
        )}
      />

      <Card className="diagnostics-privacy">
        <span><ShieldCheck size={20} /></span>
        <div>
          <strong>{t("diagnostics.privacyTitle")}</strong>
          <p>{t("diagnostics.privacyDetail")}</p>
        </div>
      </Card>

      <Card className="diagnostics-controls">
        <form className="diagnostics-filter-grid" onSubmit={applySearch}>
          <Field label={t("diagnostics.minimumLevel")}>
            <Select value={level} onChange={(event) => setLevel(event.target.value as LevelFilter)}>
              <option value="all">{t("diagnostics.allLevels")}</option>
              <option value="trace">{t("diagnostics.level.trace")}</option>
              <option value="debug">{t("diagnostics.level.debug")}</option>
              <option value="info">{t("diagnostics.level.info")}</option>
              <option value="warn">{t("diagnostics.level.warn")}</option>
              <option value="error">{t("diagnostics.level.error")}</option>
            </Select>
          </Field>
          <Field label={t("diagnostics.component")} hint={t("diagnostics.componentHint")}>
            <Input
              value={target}
              onChange={(event) => setTarget(event.target.value)}
              placeholder="audiobookai_service"
            />
          </Field>
          <Field label={t("common.search")} hint={t("diagnostics.searchHint")}>
            <div className="diagnostics-search">
              <Input
                value={searchDraft}
                onChange={(event) => setSearchDraft(event.target.value)}
                placeholder={t("diagnostics.searchPlaceholder")}
              />
              <Button type="submit" variant="secondary" aria-label={t("diagnostics.applySearch")}>
                <Search size={16} />
              </Button>
            </div>
          </Field>
        </form>
        <label className="diagnostics-follow">
          <input
            type="checkbox"
            checked={autoRefresh}
            onChange={(event) => setAutoRefresh(event.target.checked)}
          />
          <span>{t("diagnostics.autoRefresh")}</span>
        </label>
      </Card>

      {download.isError ? <div className="diagnostics-inline-error" role="alert">{t("diagnostics.exportFailed")}</div> : null}
      {diagnostics.isLoading ? <LoadingState label={t("state.loadingDiagnostics")} /> : null}
      {diagnostics.isError ? <ErrorState error={diagnostics.error} onRetry={() => diagnostics.refetch()} /> : null}
      {diagnostics.data && !diagnostics.data.items.length ? (
        <EmptyState icon="offline" title={t("diagnostics.emptyTitle")} detail={t("diagnostics.emptyDetail")} />
      ) : null}
      {diagnostics.data?.items.length ? (
        <section className="diagnostics-results" aria-label={t("diagnostics.results")}>
          <div className="diagnostics-result-meta">
            <span>{t("diagnostics.resultCount", { count: diagnostics.data.total })}</span>
            <span>{t("diagnostics.latestSequence", { sequence: diagnostics.data.latestSequence })}</span>
          </div>
          <div className="diagnostics-log">
            {diagnostics.data.items.map((entry) => (
              <DiagnosticRow key={entry.sequence} entry={entry} locale={i18n.language} />
            ))}
          </div>
        </section>
      ) : null}
    </>
  );
}

function DiagnosticRow({ entry, locale }: { entry: DiagnosticEntry; locale: string }) {
  const { t } = useTranslation();
  const details = Object.entries(entry.fields);
  return (
    <article className={`diagnostic-row diagnostic-${entry.level}`}>
      <div className="diagnostic-row-meta">
        <Badge tone={levelTone[entry.level]}>{t(`diagnostics.level.${entry.level}`)}</Badge>
        <time dateTime={entry.timestamp}>{formatTimestamp(entry.timestamp, locale)}</time>
        <span>#{entry.sequence}</span>
      </div>
      <div className="diagnostic-row-body">
        <div className="diagnostic-target"><Bug size={14} />{entry.target}</div>
        <strong>{entry.message}</strong>
        {details.length ? (
          <dl>
            {details.map(([name, value]) => (
              <div key={name}>
                <dt>{name}</dt>
                <dd>{String(value)}</dd>
              </div>
            ))}
          </dl>
        ) : null}
      </div>
    </article>
  );
}

function formatTimestamp(value: string, locale: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return new Intl.DateTimeFormat(locale, {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    fractionalSecondDigits: 3,
  }).format(date);
}
