import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, Check, CheckCircle2, ExternalLink, FileAudio, FileCheck2, LoaderCircle, PackageCheck, Play, Save, ShieldCheck, XCircle } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ApiError, api, distributionReportHtmlUrl } from "../api/client";
import type { DistributionMetadata, DistributionMetadataView, DistributionTarget, ExportArtifact, QualityFinding } from "../api/types";
import { EmptyState, ErrorState, LoadingState } from "../components/StateViews";
import { Badge, Button, Card, Field, Input, Select, SwitchField, Textarea } from "../components/ui";
import { formatBytes, formatDate } from "../lib/format";

const emptyMetadata: DistributionMetadata = {
  authors: [], narrators: [], opening_credit_segment_ids: [], closing_credit_segment_ids: [], sample_segment_ids: [], attestations: {},
};

type MetadataListKey = "authors" | "narrators" | "opening_credit_segment_ids" | "closing_credit_segment_ids" | "sample_segment_ids";

function metadataListDrafts(metadata: DistributionMetadata): Record<MetadataListKey, string> {
  return {
    authors: metadata.authors.join("\n"),
    narrators: metadata.narrators.join("\n"),
    opening_credit_segment_ids: metadata.opening_credit_segment_ids.join(", "),
    closing_credit_segment_ids: metadata.closing_credit_segment_ids.join(", "),
    sample_segment_ids: metadata.sample_segment_ids.join(", "),
  };
}

function csv(value: string): string[] {
  return [...new Set(value.split(/[,\n]/).map((part) => part.trim()).filter(Boolean))];
}

function peopleLines(value: string): string[] {
  return [...new Set(value.split(/\r?\n/).map((part) => part.trim()).filter(Boolean))];
}

interface ExportGroup {
  jobId: string;
  artifacts: ExportArtifact[];
  partCount: number;
  complete: boolean;
  hasManifest: boolean;
}

function groupExports(artifacts: ExportArtifact[]): ExportGroup[] {
  const byJob = new Map<string, ExportArtifact[]>();
  for (const artifact of artifacts) byJob.set(artifact.jobId, [...(byJob.get(artifact.jobId) ?? []), artifact]);
  return [...byJob.entries()].map(([jobId, jobArtifacts]) => {
    const sorted = [...jobArtifacts].sort((left, right) => left.partIndex - right.partIndex);
    const partCount = sorted[0]?.partCount ?? 0;
    const complete = partCount > 0
      && sorted.length === partCount
      && sorted.every((artifact, index) => artifact.partCount === partCount && artifact.partIndex === index);
    return {
      jobId,
      artifacts: sorted,
      partCount,
      complete,
      hasManifest: sorted.length > 0 && sorted.every((artifact) => Boolean(artifact.manifestUrl?.trim())),
    };
  }).sort((left, right) => Date.parse(right.artifacts[0]?.createdAt ?? "") - Date.parse(left.artifacts[0]?.createdAt ?? ""));
}

function optional(value: string): string | undefined {
  return value.trim() || undefined;
}

function findingTone(status: QualityFinding["status"]): "neutral" | "warning" | "danger" | "positive" {
  if (status === "fail") return "danger";
  if (status === "warning" || status === "manual") return "warning";
  if (status === "pass") return "positive";
  return "neutral";
}

function isStaleMetadata(error: unknown): boolean {
  return error instanceof ApiError && error.problem.code === "stale_distribution_metadata";
}

export function DistributionPanel({ projectId }: { projectId: string }) {
  const { t, i18n } = useTranslation();
  const queryClient = useQueryClient();
  const policies = useQuery({ queryKey: ["distribution", "policies"], queryFn: api.distributionPolicies });
  const metadataQuery = useQuery({ queryKey: ["distribution", projectId, "metadata"], queryFn: () => api.distributionMetadata(projectId) });
  const packages = useQuery({ queryKey: ["distribution", projectId, "packages"], queryFn: () => api.distributionPackages(projectId) });
  const exportsQuery = useQuery({ queryKey: ["exports"], queryFn: api.exports });
  const [metadata, setMetadata] = useState<DistributionMetadata>(emptyMetadata);
  const [listDrafts, setListDrafts] = useState(() => metadataListDrafts(emptyMetadata));
  const [target, setTarget] = useState<DistributionTarget>("generic_m4b");
  const [selectedExportJobId, setSelectedExportJobId] = useState("");
  const [reviewIds, setReviewIds] = useState("");
  const [metadataOpen, setMetadataOpen] = useState(true);
  const [metadataConflict, setMetadataConflict] = useState(false);
  const [metadataBaseProjectId, setMetadataBaseProjectId] = useState(projectId);
  const [metadataBaseRevision, setMetadataBaseRevision] = useState<number>();
  const [metadataBase, setMetadataBase] = useState<DistributionMetadata>();
  const highestObservedRevision = useRef({ projectId, revision: -1 });
  if (highestObservedRevision.current.projectId !== projectId) highestObservedRevision.current = { projectId, revision: -1 };
  if (metadataQuery.data) highestObservedRevision.current.revision = Math.max(highestObservedRevision.current.revision, metadataQuery.data.revision);
  const projectExports = useMemo(() => exportsQuery.data?.items.filter((artifact) => artifact.projectId === projectId) ?? [], [exportsQuery.data, projectId]);
  const exportGroups = useMemo(() => groupExports(projectExports), [projectExports]);
  const selectedExportGroup = exportGroups.find((group) => group.jobId === selectedExportJobId && group.complete && group.hasManifest);
  const uploadIds = selectedExportGroup?.artifacts.map((artifact) => artifact.id) ?? [];
  const activePolicy = policies.data?.items.find((policy) => policy.target === target);
  const metadataForSave = (): DistributionMetadata => ({
    ...metadata,
    authors: peopleLines(listDrafts.authors),
    narrators: peopleLines(listDrafts.narrators),
    opening_credit_segment_ids: csv(listDrafts.opening_credit_segment_ids),
    closing_credit_segment_ids: csv(listDrafts.closing_credit_segment_ids),
    sample_segment_ids: csv(listDrafts.sample_segment_ids),
  });
  const metadataDirty = metadataBase ? JSON.stringify(metadataForSave()) !== JSON.stringify(metadataBase) : false;
  const adoptMetadata = useCallback((view: DistributionMetadataView): boolean => {
    if (view.revision < highestObservedRevision.current.revision) return false;
    highestObservedRevision.current = { projectId, revision: view.revision };
    setMetadata(view.metadata);
    setListDrafts(metadataListDrafts(view.metadata));
    setMetadataBaseProjectId(projectId);
    setMetadataBaseRevision(view.revision);
    setMetadataBase(view.metadata);
    setMetadataConflict(false);
    return true;
  }, [projectId]);

  useEffect(() => {
    const current = metadataQuery.data;
    if (!current) return;
    if (metadataBaseProjectId !== projectId || metadataBaseRevision == null || !metadataDirty) {
      adoptMetadata(current);
    } else if (current.revision !== metadataBaseRevision) {
      // Keep the user's draft. Saving remains pinned to metadataBaseRevision so the
      // service CAS can never apply it on top of an unseen revision.
      setMetadataConflict(true);
    }
  }, [adoptMetadata, metadataBaseProjectId, metadataBaseRevision, metadataDirty, metadataQuery.data, projectId]);
  const saveMetadata = useMutation({
    mutationFn: () => api.updateDistributionMetadata(projectId, metadataBaseRevision ?? 0, metadataForSave()),
    onMutate: () => setMetadataConflict(false),
    onError: (error) => setMetadataConflict(isStaleMetadata(error)),
    onSuccess: async (data) => {
      if (adoptMetadata(data)) queryClient.setQueryData(["distribution", projectId, "metadata"], data);
      await queryClient.invalidateQueries({ queryKey: ["distribution", projectId, "packages"] });
    },
  });
  const createPackage = useMutation({
    mutationFn: () => api.createDistributionPackage(projectId, target, uploadIds, csv(reviewIds)),
    onSuccess: async () => {
      setSelectedExportJobId("");
      setReviewIds("");
      await queryClient.invalidateQueries({ queryKey: ["distribution", projectId, "packages"] });
    },
  });
  const runQc = useMutation({
    mutationFn: (packageId: string) => {
      if (metadataDirty) throw new Error(t("distribution.saveBeforeQc"));
      return api.runDistributionQualityControl(packageId);
    },
    onSuccess: async () => { await queryClient.invalidateQueries({ queryKey: ["distribution", projectId, "packages"] }); },
  });

  if (policies.isLoading || metadataQuery.isLoading || packages.isLoading || exportsQuery.isLoading) return <LoadingState label={t("distribution.loading")} />;
  if (policies.isError) return <ErrorState error={policies.error} onRetry={() => void policies.refetch()} />;
  if (metadataQuery.isError) return <ErrorState error={metadataQuery.error} onRetry={() => void metadataQuery.refetch()} />;
  if (packages.isError) return <ErrorState error={packages.error} onRetry={() => void packages.refetch()} />;
  if (exportsQuery.isError) return <ErrorState error={exportsQuery.error} onRetry={() => void exportsQuery.refetch()} />;

  const setText = (key: keyof DistributionMetadata, value: string) => setMetadata((current) => ({ ...current, [key]: optional(value) }));
  const setList = (key: MetadataListKey, value: string) => setListDrafts((current) => ({ ...current, [key]: value }));
  const setAttestation = (key: keyof DistributionMetadata["attestations"], enabled: boolean) => setMetadata((current) => {
    const attestations = { ...current.attestations };
    if (key === "acx_external_authorization") {
      attestations.acx_external_authorization = enabled ? new Date().toISOString() : undefined;
      if (!enabled) attestations.acx_authorization_reference = undefined;
    } else {
      attestations[key] = enabled ? new Date().toISOString() : undefined;
    }
    return { ...current, attestations };
  });
  const reloadMetadataAfterConflict = async () => {
    const fresh = await metadataQuery.refetch();
    if (fresh.data) adoptMetadata(fresh.data);
    saveMetadata.reset();
  };

  return (
    <div className="distribution-workbench stack">
      <div className="section-heading distribution-heading"><div><p className="eyebrow">{t("distribution.eyebrow")}</p><h2>{t("distribution.title")}</h2><p>{t("distribution.workspaceSubtitle")}</p></div><Badge tone="accent"><ShieldCheck size={13} />{t("distribution.policySnapshot")}</Badge></div>

      <Card className="distribution-policy stack">
        <div className="distribution-policy-top">
          <Field label={t("distribution.target")}><Select value={target} onChange={(event) => setTarget(event.target.value as DistributionTarget)}>{policies.data?.items.map((policy) => <option value={policy.target} key={policy.target}>{policy.displayName}</option>)}</Select></Field>
          {activePolicy ? <div className="distribution-policy-version"><span>{t("distribution.policyVersion")}</span><strong>{activePolicy.policyVersion}</strong><small>{t("distribution.effective", { date: activePolicy.effectiveDate })}</small></div> : null}
        </div>
        {activePolicy ? <><div className="distribution-rule-list">{activePolicy.rules.map((rule) => <div className="distribution-rule" key={rule.code}><Badge tone={rule.level === "required" ? "danger" : rule.level === "manual_gate" ? "warning" : "neutral"}>{t(`distribution.level.${rule.level}`)}</Badge><div><strong>{rule.description}</strong><small>{rule.automated ? t("distribution.automated") : t("distribution.manualCheck")}</small></div></div>)}</div><div className="cluster">{activePolicy.sourceUrls.map((url, index) => <a className="button button-ghost button-sm" href={url} target="_blank" rel="noreferrer" key={url}><ExternalLink size={14} />{t("distribution.source", { count: index + 1 })}</a>)}</div></> : null}
      </Card>

      <Card className="distribution-metadata stack">
        <button type="button" className="distribution-section-toggle space-between" aria-expanded={metadataOpen} onClick={() => setMetadataOpen((value) => !value)}><span><strong>{t("distribution.metadata")}</strong><small>{t("distribution.metadataDetail")}</small></span><Badge>{t("distribution.revision", { count: metadataQuery.data?.revision ?? 0 })}</Badge></button>
        {metadataOpen ? <>
          <div className="distribution-metadata-grid">
            <Field label={t("distribution.authors")} hint={t("distribution.peopleListHint")}><Textarea value={listDrafts.authors} onChange={(event) => setList("authors", event.target.value)} /></Field>
            <Field label={t("distribution.narrators")} hint={t("distribution.peopleListHint")}><Textarea value={listDrafts.narrators} onChange={(event) => setList("narrators", event.target.value)} /></Field>
            <Field label={t("distribution.subtitle")}><Input value={metadata.subtitle ?? ""} onChange={(event) => setText("subtitle", event.target.value)} /></Field>
            <Field label={t("distribution.language")}><Input value={metadata.language ?? ""} onChange={(event) => setText("language", event.target.value)} placeholder="en" /></Field>
            <Field label={t("distribution.publisher")}><Input value={metadata.publisher ?? ""} onChange={(event) => setText("publisher", event.target.value)} /></Field>
            <Field label={t("distribution.imprint")}><Input value={metadata.imprint ?? ""} onChange={(event) => setText("imprint", event.target.value)} /></Field>
            <Field label={t("distribution.identifier")}><Input value={metadata.identifier ?? ""} onChange={(event) => setText("identifier", event.target.value)} /></Field>
            <Field label={t("distribution.identifierKind")}><Input value={metadata.identifier_kind ?? ""} onChange={(event) => setText("identifier_kind", event.target.value)} placeholder="ISBN" /></Field>
            <Field label={t("distribution.releaseDate")}><Input type="date" value={metadata.release_date ?? ""} onChange={(event) => setText("release_date", event.target.value)} /></Field>
            <Field label={t("distribution.abridged")}><Select value={metadata.abridged == null ? "" : String(metadata.abridged)} onChange={(event) => setMetadata((current) => ({ ...current, abridged: event.target.value === "" ? undefined : event.target.value === "true" }))}><option value="">{t("distribution.unspecified")}</option><option value="false">{t("distribution.unabridged")}</option><option value="true">{t("distribution.abridgedYes")}</option></Select></Field>
            <Field label={t("distribution.coverArtifactId")}><Input value={metadata.cover_artifact_id ?? ""} onChange={(event) => setText("cover_artifact_id", event.target.value)} /></Field>
            <Field label={t("distribution.sourceRights")}><Input value={metadata.source_rights ?? ""} onChange={(event) => setText("source_rights", event.target.value)} /></Field>
            <Field label={t("distribution.audioRights")}><Input value={metadata.audio_rights ?? ""} onChange={(event) => setText("audio_rights", event.target.value)} /></Field>
            <Field label={t("distribution.openingCredits")} hint={t("distribution.idsHint")}><Textarea value={listDrafts.opening_credit_segment_ids} onChange={(event) => setList("opening_credit_segment_ids", event.target.value)} /></Field>
            <Field label={t("distribution.closingCredits")} hint={t("distribution.idsHint")}><Textarea value={listDrafts.closing_credit_segment_ids} onChange={(event) => setList("closing_credit_segment_ids", event.target.value)} /></Field>
            <Field label={t("distribution.samples")} hint={t("distribution.idsHint")}><Textarea value={listDrafts.sample_segment_ids} onChange={(event) => setList("sample_segment_ids", event.target.value)} /></Field>
            <Field label={t("distribution.description")} className="distribution-description"><Textarea value={metadata.description ?? ""} onChange={(event) => setText("description", event.target.value)} /></Field>
          </div>
          <div className="distribution-attestations stack"><h3>{t("distribution.attestations")}</h3><p>{t("distribution.attestationsDetail")}</p>
            <SwitchField checked={Boolean(metadata.attestations.rights_and_eligibility_confirmed)} onCheckedChange={(value) => setAttestation("rights_and_eligibility_confirmed", value)} label={t("distribution.rightsConfirmed")} detail={metadata.attestations.rights_and_eligibility_confirmed ? formatDate(metadata.attestations.rights_and_eligibility_confirmed, i18n.language) : undefined} />
            <SwitchField checked={Boolean(metadata.attestations.spotify_digital_voice_disclosure)} onCheckedChange={(value) => setAttestation("spotify_digital_voice_disclosure", value)} label={t("distribution.spotifyDisclosure")} detail={t("distribution.spotifyDisclosureDetail")} />
            <SwitchField checked={Boolean(metadata.attestations.acx_external_authorization)} onCheckedChange={(value) => setAttestation("acx_external_authorization", value)} label={t("distribution.acxAuthorization")} detail={t("distribution.acxAuthorizationDetail")} />
            {metadata.attestations.acx_external_authorization ? <Field label={t("distribution.acxReference")}><Input value={metadata.attestations.acx_authorization_reference ?? ""} onChange={(event) => setMetadata((current) => ({ ...current, attestations: { ...current.attestations, acx_authorization_reference: optional(event.target.value) } }))} /></Field> : null}
          </div>
          {metadataConflict ? <Card className="inline-notice"><AlertTriangle size={18} /><div><strong>{t("distribution.metadataConflict")}</strong><p>{t("distribution.metadataConflictHint")}</p></div><Button variant="secondary" size="sm" onClick={() => void reloadMetadataAfterConflict()}>{t("distribution.reloadMetadata")}</Button></Card> : null}
          {saveMetadata.isError && !metadataConflict ? <ErrorState error={saveMetadata.error} onRetry={() => saveMetadata.mutate()} /> : null}
          <div className="panel-footer"><Button disabled={saveMetadata.isPending || !metadataDirty} onClick={() => saveMetadata.mutate()}>{saveMetadata.isPending ? <LoaderCircle className="spin" size={16} /> : <Save size={16} />}{t("distribution.saveMetadata")}</Button></div>
        </> : null}
      </Card>

      <Card className="distribution-package-builder stack">
        <div><h2>{t("distribution.createPackage")}</h2><p>{t("distribution.createPackageDetail")}</p></div>
        {!exportGroups.length ? <EmptyState title={t("distribution.noExports")} detail={t("distribution.noExportsDetail")} /> : <div className="distribution-artifact-list">{exportGroups.map((group) => {
          const eligible = group.complete && group.hasManifest;
          return <label className="distribution-artifact distribution-export-group" key={group.jobId}><input type="radio" name="distribution-export-job" value={group.jobId} checked={selectedExportJobId === group.jobId} disabled={!eligible || createPackage.isPending} onChange={() => setSelectedExportJobId(group.jobId)} /><span className="distribution-artifact-icon"><FileAudio size={17} /></span><span className="stack"><span><strong>{t("distribution.exportGroup", { date: formatDate(group.artifacts[0]?.createdAt ?? "", i18n.language) })}</strong><small>{t("distribution.exportGroupCount", { count: group.artifacts.length })}</small></span><ul className="distribution-export-contents">{group.artifacts.map((artifact) => <li key={artifact.id}><span>{artifact.fileName}</span><small>{artifact.format.toUpperCase()} · {formatBytes(artifact.sizeBytes, i18n.language)} · {t("distribution.exportPart", { part: artifact.partIndex + 1, count: artifact.partCount })}</small></li>)}</ul>{!group.complete ? <small className="field-error">{t("distribution.incompleteExportGroup", { found: group.artifacts.length, expected: group.partCount })}</small> : !group.hasManifest ? <small className="field-error">{t("distribution.missingExportManifest")}</small> : null}</span></label>;
        })}</div>}
        <details className="distribution-advanced-artifacts"><summary>{t("distribution.advancedArtifacts")}</summary><Field label={t("distribution.reviewArtifacts")} hint={t("distribution.reviewArtifactsHint")}><Input value={reviewIds} onChange={(event) => setReviewIds(event.target.value)} /></Field></details>
        {createPackage.isError ? <ErrorState error={createPackage.error} onRetry={() => createPackage.mutate()} /> : null}
        <div className="panel-footer"><Button disabled={!uploadIds.length || createPackage.isPending} onClick={() => createPackage.mutate()}>{createPackage.isPending ? <LoaderCircle className="spin" size={16} /> : <PackageCheck size={16} />}{t("distribution.createPackageAction")}</Button></div>
      </Card>

      <section className="distribution-packages" aria-labelledby="distribution-packages-title">
        <div className="section-heading"><div><h2 id="distribution-packages-title">{t("distribution.packages")}</h2><p>{t("distribution.packagesDetail")}</p></div></div>
        {metadataDirty ? <Card className="inline-notice" role="status"><AlertTriangle size={18} /><div><strong>{t("distribution.unsavedMetadata")}</strong><p>{t("distribution.saveBeforeQc")}</p></div></Card> : null}
        {!packages.data?.items.length ? <EmptyState title={t("distribution.noPackages")} detail={t("distribution.noPackagesDetail")} /> : <div className="distribution-package-list">{packages.data.items.map((item) => {
          const report = item.latestReport;
          const findings = report?.findings.filter((finding) => finding.status !== "pass") ?? [];
          const reportCurrent = Boolean(report && item.latestReportCurrent);
          return <Card className="distribution-package stack" key={item.package.id}>
            <div className="space-between distribution-package-head"><div className="cluster"><span className="distribution-package-icon"><FileCheck2 size={19} /></span><div><strong>{policies.data?.items.find((policy) => policy.target === item.package.target)?.displayName ?? item.package.target}</strong><p>{formatDate(item.package.created_at, i18n.language)} · {item.package.upload_artifact_ids.length} {t("distribution.files")}</p></div></div><div className="cluster">{reportCurrent && report ? <><Badge tone={report.technical_ready ? "positive" : "danger"}>{report.technical_ready ? <CheckCircle2 size={12} /> : <XCircle size={12} />}{t("distribution.technical")}</Badge><Badge tone={report.submission_ready ? "positive" : "warning"}>{report.submission_ready ? <Check size={12} /> : <AlertTriangle size={12} />}{t("distribution.submission")}</Badge></> : report ? <Badge tone="warning"><AlertTriangle size={12} />{t("distribution.staleReport")}</Badge> : <Badge>{t("distribution.notChecked")}</Badge>}</div></div>
            <code className="distribution-output">{item.package.output_directory}</code>
            {report ? <div className="distribution-report"><div className="space-between"><div><strong>{t("distribution.latestReport")}</strong><p>{t("distribution.generated", { date: formatDate(report.generated_at, i18n.language) })}</p></div><a className="button button-secondary button-sm" href={distributionReportHtmlUrl(report.id)} target="_blank" rel="noreferrer"><ExternalLink size={14} />{t("distribution.openHtml")}</a></div>{!reportCurrent ? <Card className="inline-notice"><AlertTriangle size={18} /><div><strong>{t("distribution.staleReport")}</strong><p>{t("distribution.staleReportDetail")}</p></div></Card> : findings.length ? <ul className="distribution-findings">{findings.map((finding) => <li key={`${finding.code}-${finding.scope}`}><Badge tone={findingTone(finding.status)}>{t(`distribution.finding.${finding.status}`)}</Badge><div><strong>{finding.message}</strong><small>{finding.scope}</small>{finding.remediation ? <p>{finding.remediation}</p> : null}</div></li>)}</ul> : <p className="distribution-clean"><ShieldCheck size={16} />{t("distribution.noBlockingFindings")}</p>}</div> : <Card className="inline-notice"><AlertTriangle size={18} /><p>{t("distribution.runFirstQc")}</p></Card>}
            {runQc.isError && runQc.variables === item.package.id ? <ErrorState error={runQc.error} /> : null}
            <div className="panel-footer"><Button variant="secondary" disabled={runQc.isPending || metadataDirty || saveMetadata.isPending} onClick={() => runQc.mutate(item.package.id)}>{runQc.isPending && runQc.variables === item.package.id ? <LoaderCircle className="spin" size={15} /> : <Play size={15} />}{report ? t("distribution.rerunQc") : t("distribution.runQc")}</Button></div>
          </Card>;
        })}</div>}
      </section>
    </div>
  );
}
