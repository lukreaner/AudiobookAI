import { useQuery } from "@tanstack/react-query";
import { BookAudio, CheckCircle2, Download, FileJson2, HardDrive, ListMusic } from "lucide-react";
import { useTranslation } from "react-i18next";
import { api } from "../api/client";
import { EmptyState, ErrorState, LoadingState } from "../components/StateViews";
import { Badge, Card, PageHeading } from "../components/ui";
import { formatBytes, formatDate, formatDuration } from "../lib/format";

export function ExportsPage() {
  const { t, i18n } = useTranslation();
  const exportsQuery = useQuery({ queryKey: ["exports"], queryFn: api.exports });
  if (exportsQuery.isLoading) return <LoadingState label={t("common.loading")} />;
  if (exportsQuery.isError) return <ErrorState error={exportsQuery.error} onRetry={() => void exportsQuery.refetch()} />;
  return (
    <div className="page exports-page">
      <PageHeading eyebrow={t("exports.eyebrow")} title={t("exports.title")} subtitle={t("exports.subtitle")} />
      {!exportsQuery.data?.items.length ? <EmptyState title={t("exports.emptyTitle")} detail={t("exports.emptyDetail")} /> : (
        <div className="export-grid">
          {exportsQuery.data.items.map((artifact) => (
            <Card className="export-card" key={artifact.id}>
              <div className="export-art"><BookAudio size={28} /><span>{artifact.format.toUpperCase()}</span></div>
              <div className="export-body">
                <div className="space-between"><Badge tone="positive"><CheckCircle2 size={12} />{t("jobs.complete")}</Badge><span className="export-date">{t("exports.created", { value: formatDate(artifact.createdAt, i18n.language) })}</span></div>
                <h2>{artifact.projectTitle}</h2><p className="export-name">{artifact.fileName}</p>
                <div className="export-meta"><span><BookAudio size={14} />{t("exports.format", { format: artifact.format.toUpperCase() })}</span><span><HardDrive size={14} />{formatBytes(artifact.sizeBytes, i18n.language)}</span><span><ListMusic size={14} />{artifact.splitMode === "single" ? t("exports.single") : t("exports.perChapter")}</span><span>{formatDuration(artifact.durationSeconds, i18n.language)}</span></div>
                <div className="export-markers"><Badge tone={artifact.chapterMarkers ? "accent" : "neutral"}>{artifact.chapterMarkers ? t("exports.markers") : t("exports.noMarkers")}</Badge></div>
                <div className="export-actions"><a className="button button-primary button-sm" href={artifact.downloadUrl} download><Download size={15} />{t("exports.download")}</a><a className="button button-ghost button-sm" href={artifact.manifestUrl} target="_blank" rel="noreferrer"><FileJson2 size={15} />{t("exports.manifest")}</a></div>
              </div>
            </Card>
          ))}
        </div>
      )}
    </div>
  );
}
