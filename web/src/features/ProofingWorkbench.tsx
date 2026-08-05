import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { clsx } from "clsx";
import { AlertTriangle, Check, CircleAlert, Clock3, FileAudio, Flag, Headphones, LoaderCircle, Lock, Play, RefreshCw, Save, Search, ShieldCheck, Sparkles, Volume2 } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";
import { ApiError, api, artifactUrl } from "../api/client";
import type { Character, DeliveryCue, ExportFormat, ModelPerformanceCapabilities, PerformanceRange, PerformanceSettings, ProductionSegment, ProofingSegmentView, ProviderProfile, SegmentReviewState, TimingSettings } from "../api/types";
import { ErrorState, EmptyState, LoadingState } from "../components/StateViews";
import { Badge, Button, Card, Field, Input, Select, Stat, SwitchField, Textarea } from "../components/ui";
import { DEFAULT_EXPORT_SETTINGS, toJobExportSettings } from "./exportSettings";
import { formatCount, formatDate, formatMoney } from "../lib/format";

function optionalNumber(value: string): number | undefined {
  if (!value.trim()) return undefined;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : undefined;
}

function settingsEqual(left: PerformanceSettings | TimingSettings, right: PerformanceSettings | TimingSettings): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function speakerCharacter(view: ProofingSegmentView, characters: Character[]): Character | undefined {
  const speaker = view.segment.speaker;
  if (speaker.kind === "narrator") return characters.find((character) => character.role === "narrator");
  if (speaker.kind === "character") return characters.find((character) => character.id === speaker.id);
  return undefined;
}

function speakerLabel(view: ProofingSegmentView, characters: Character[], narratorLabel: string): string {
  const character = speakerCharacter(view, characters);
  if (character) return character.canonicalName;
  const speaker = view.segment.speaker;
  return speaker.kind === "narrator" ? narratorLabel : speaker.id;
}

function providerCapabilitiesFresh(provider: ProviderProfile): boolean {
  const updatedAt = provider.capabilityUpdatedAt ? Date.parse(provider.capabilityUpdatedAt) : Number.NaN;
  return provider.status === "online" && Number.isFinite(updatedAt) && updatedAt + 24 * 60 * 60 * 1000 > Date.now();
}

function descriptorFor(view: ProofingSegmentView, characters: Character[], providers: ProviderProfile[]): ModelPerformanceCapabilities | undefined {
  const assignment = speakerCharacter(view, characters)?.voiceAssignment;
  if (!assignment) return undefined;
  const provider = providers.find((item) => item.id === assignment.providerProfileId);
  const model = assignment.model?.trim() || provider?.model?.trim();
  if (!provider || !model || !providerCapabilitiesFresh(provider)) return undefined;
  return provider.capabilities?.modelPerformance.find((descriptor) => descriptor.model === model);
}

function supportedPerformance(settings: PerformanceSettings, descriptor?: ModelPerformanceCapabilities): PerformanceSettings {
  if (!descriptor) return {};
  const capability = descriptor.performance;
  return {
    ...(capability.speed && settings.speed != null ? { speed: settings.speed } : {}),
    ...(capability.pitch && settings.pitch != null ? { pitch: settings.pitch } : {}),
    ...(capability.stability && settings.stability != null ? { stability: settings.stability } : {}),
    ...(capability.similarity && settings.similarity != null ? { similarity: settings.similarity } : {}),
    ...(capability.style && settings.style != null ? { style: settings.style } : {}),
    ...(capability.speaker_boost && settings.speaker_boost != null ? { speaker_boost: settings.speaker_boost } : {}),
    ...(settings.delivery_cue && capability.delivery_cues.includes(settings.delivery_cue) ? { delivery_cue: settings.delivery_cue } : {}),
  };
}

function hasUnsupportedPerformance(settings: PerformanceSettings, descriptor?: ModelPerformanceCapabilities): boolean {
  if (!descriptor) return Object.values(settings).some((value) => value != null);
  const capability = descriptor.performance;
  return (settings.speed != null && !capability.speed)
    || (settings.pitch != null && !capability.pitch)
    || (settings.stability != null && !capability.stability)
    || (settings.similarity != null && !capability.similarity)
    || (settings.style != null && !capability.style)
    || (settings.speaker_boost != null && !capability.speaker_boost)
    || (settings.delivery_cue != null && !capability.delivery_cues.includes(settings.delivery_cue));
}

function rangeHint(range?: PerformanceRange | null): string {
  return range ? `${range.minimum}–${range.maximum}` : "—";
}

function segmentDraftBase(segment: ProductionSegment) {
  return {
    revision: segment.revision,
    textOverride: segment.narration_text_override ?? "",
    performance: segment.performance_override,
    timing: segment.timing_override,
  };
}

function needsFreshEstimate(error: unknown): boolean {
  return error instanceof ApiError && ["estimate_expired", "estimate_changed", "stale_segment_revision"].includes(error.problem.code ?? "");
}

function reviewTone(state: SegmentReviewState): "neutral" | "warning" | "positive" | "accent" {
  if (state === "flagged") return "warning";
  if (state === "approved") return "positive";
  if (state === "locked") return "accent";
  return "neutral";
}

export function ProofingWorkbench({ projectId }: { projectId: string }) {
  const { t, i18n } = useTranslation();
  const queryClient = useQueryClient();
  const [chapterId, setChapterId] = useState("");
  const [reviewState, setReviewState] = useState<"" | SegmentReviewState>("");
  const [issueFilter, setIssueFilter] = useState<"all" | "issues" | "stale">("all");
  const [search, setSearch] = useState("");
  const [strictRetailer, setStrictRetailer] = useState(false);
  const [exportFormat, setExportFormat] = useState<ExportFormat>(DEFAULT_EXPORT_SETTINGS.format);
  const [splitPerChapter, setSplitPerChapter] = useState(DEFAULT_EXPORT_SETTINGS.splitPerChapter);
  const [bitrateKbps, setBitrateKbps] = useState(DEFAULT_EXPORT_SETTINGS.bitrateKbps);
  const [dirtySegmentIds, setDirtySegmentIds] = useState<Set<string>>(() => new Set());

  const handleSegmentDirty = useCallback((segmentId: string, dirty: boolean) => {
    setDirtySegmentIds((current) => {
      if (current.has(segmentId) === dirty) return current;
      const next = new Set(current);
      if (dirty) next.add(segmentId);
      else next.delete(segmentId);
      return next;
    });
  }, []);

  const summary = useQuery({ queryKey: ["proofing", projectId, "summary"], queryFn: () => api.proofingSummary(projectId) });
  const characters = useQuery({ queryKey: ["characters", projectId], queryFn: () => api.characters(projectId), enabled: summary.data?.available === true });
  const providers = useQuery({ queryKey: ["providers"], queryFn: api.providers, enabled: summary.data?.available === true });
  const segmentQuery = {
    chapterId: chapterId || undefined,
    state: reviewState || undefined,
    issuesOnly: issueFilter === "issues" || undefined,
    staleOnly: issueFilter === "stale" || undefined,
    search: search || undefined,
    limit: 250,
  };
  const segments = useInfiniteQuery({
    queryKey: ["proofing", projectId, "segments", segmentQuery],
    initialPageParam: "",
    queryFn: ({ pageParam }) => api.proofingSegments(projectId, { ...segmentQuery, cursor: pageParam || undefined }),
    getNextPageParam: (lastPage) => lastPage.nextCursor || undefined,
    enabled: summary.data?.available === true,
  });
  const startExport = useMutation({
    mutationFn: () => api.startProofingExport(projectId, strictRetailer, toJobExportSettings({
      ...DEFAULT_EXPORT_SETTINGS,
      format: exportFormat,
      splitPerChapter,
      bitrateKbps,
    })),
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["jobs"] }),
        queryClient.invalidateQueries({ queryKey: ["proofing", projectId] }),
      ]);
    },
  });

  const refreshProofing = async () => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ["proofing", projectId] }),
      queryClient.invalidateQueries({ queryKey: ["jobs"] }),
    ]);
  };

  if (summary.isLoading) return <LoadingState label={t("proofing.loading")} />;
  if (summary.isError) return <ErrorState error={summary.error} onRetry={() => void summary.refetch()} />;
  if (!summary.data?.available) return <EmptyState title={t("proofing.unavailableTitle")} detail={t("proofing.unavailableDetail")} action={<Link className="button button-primary button-md" to={`/projects/${projectId}/preflight`}>{t("proofing.startConversion")}</Link>} />;

  const counts = summary.data.counts;
  const exportReady = strictRetailer ? summary.data.retailerExportReady : summary.data.genericExportReady;
  const segmentItems = segments.data?.pages.flatMap((page) => page.items) ?? [];
  const segmentTotal = segments.data?.pages[0]?.total ?? 0;
  const filtersDisabled = dirtySegmentIds.size > 0;

  return (
    <div className="proofing-workbench stack">
      <div className="section-heading proofing-heading">
        <div><p className="eyebrow">{t("proofing.eyebrow")}</p><h2>{t("proofing.title")}</h2><p>{t("proofing.subtitle")}</p></div>
        <div className="cluster"><Badge tone={summary.data.plan?.status === "ready" ? "positive" : "warning"}>{t(`proofing.plan.${summary.data.plan?.status ?? "incomplete"}`)}</Badge><Button variant="secondary" size="sm" disabled={filtersDisabled} onClick={() => void refreshProofing()}><RefreshCw size={15} />{t("common.refresh")}</Button></div>
      </div>

      {summary.data.plan?.dirty_reasons.length ? <Card className="proofing-plan-warning"><AlertTriangle size={20} /><div><strong>{t("proofing.planChanged")}</strong><ul>{summary.data.plan.dirty_reasons.map((reason) => <li key={reason}>{reason}</li>)}</ul></div></Card> : null}

      <div className="proofing-stats">
        <Card><Stat label={t("proofing.total")} value={formatCount(counts.total, i18n.language)} /></Card>
        <Card><Stat label={t("proofing.unreviewed")} value={formatCount(counts.unreviewed, i18n.language)} /></Card>
        <Card><Stat label={t("proofing.flagged")} value={formatCount(counts.flagged, i18n.language)} /></Card>
        <Card><Stat label={t("proofing.approved")} value={formatCount(counts.approved + counts.locked, i18n.language)} /></Card>
        <Card className={clsx((counts.stale || counts.missing) && "proofing-stat-alert")}><Stat label={t("proofing.needsAudio")} value={formatCount(counts.stale + counts.missing, i18n.language)} detail={t("proofing.staleMissing", { stale: counts.stale, missing: counts.missing })} /></Card>
      </div>

      <Card className="proofing-toolbar">
        <Field label={t("proofing.search")} className="proofing-search"><div className="input-with-icon"><Search size={16} /><Input value={search} disabled={filtersDisabled} onChange={(event) => setSearch(event.target.value)} placeholder={t("proofing.searchPlaceholder")} /></div></Field>
        <Field label={t("proofing.chapter")}><Select value={chapterId} disabled={filtersDisabled} onChange={(event) => setChapterId(event.target.value)}><option value="">{t("proofing.allChapters")}</option>{summary.data.chapters.map((chapter) => <option value={chapter.id} key={chapter.id}>{chapter.title} · {chapter.issueCount}</option>)}</Select></Field>
        <Field label={t("proofing.reviewState")}><Select value={reviewState} disabled={filtersDisabled} onChange={(event) => setReviewState(event.target.value as "" | SegmentReviewState)}><option value="">{t("proofing.allStates")}</option>{(["unreviewed", "flagged", "approved", "locked"] as SegmentReviewState[]).map((state) => <option value={state} key={state}>{t(`proofing.state.${state}`)}</option>)}</Select></Field>
        <Field label={t("proofing.audioStatus")}><Select value={issueFilter} disabled={filtersDisabled} onChange={(event) => setIssueFilter(event.target.value as typeof issueFilter)}><option value="all">{t("proofing.allAudio")}</option><option value="issues">{t("proofing.findingsOnly")}</option><option value="stale">{t("proofing.staleOnly")}</option></Select></Field>
      </Card>

      {segments.isLoading ? <LoadingState label={t("proofing.loadingSegments")} /> : null}
      {segments.isError ? <ErrorState error={segments.error} onRetry={() => void segments.refetch()} /> : null}
      {characters.isError || providers.isError ? <Card className="inline-notice" role="alert"><AlertTriangle size={18} /><div><strong>{t("proofing.directionDataError")}</strong><p>{t("proofing.directionUnavailable")}</p></div><Button size="sm" variant="secondary" onClick={() => { void characters.refetch(); void providers.refetch(); }}>{t("common.refresh")}</Button></Card> : null}
      {segments.data && !segmentItems.length ? <EmptyState title={t("proofing.noMatches")} detail={t("proofing.noMatchesDetail")} /> : null}
      {segmentItems.length ? <div className="proofing-segment-list">{segmentItems.map((view) => <ProofingSegmentCard key={view.segment.id} projectId={projectId} view={view} speakerName={speakerLabel(view, characters.data?.items ?? [], t("characters.narratorSpeaker"))} performanceDescriptor={descriptorFor(view, characters.data?.items ?? [], providers.data?.items ?? [])} directionLookupPending={characters.isLoading || providers.isLoading} onChanged={refreshProofing} onDirtyChange={handleSegmentDirty} />)}</div> : null}
      {segments.data && segmentTotal > segmentItems.length ? <Card className="inline-notice"><CircleAlert size={18} /><div><p>{t("proofing.resultLimit", { shown: segmentItems.length, total: segmentTotal })}</p>{segments.hasNextPage ? <Button size="sm" variant="secondary" disabled={segments.isFetchingNextPage} onClick={() => void segments.fetchNextPage()}>{segments.isFetchingNextPage ? <LoaderCircle className="spin" size={14} /> : null}{segments.isFetchingNextPage ? t("proofing.loadingMore") : t("proofing.loadMore")}</Button> : null}</div></Card> : null}

      <Card className="proofing-export stack">
        <div className="space-between proofing-export-heading"><div><h2>{t("proofing.reExport")}</h2><p>{t("proofing.reExportDetail")}</p></div><Badge tone={exportReady ? "positive" : "warning"}>{exportReady ? t("proofing.ready") : t("proofing.notReady")}</Badge></div>
        <div className="proofing-export-grid">
          <Field label={t("preflight.exportFormat")}><Select value={exportFormat} onChange={(event) => setExportFormat(event.target.value as ExportFormat)}><option value="m4b">{t("preflight.formatM4b")}</option><option value="mp3">{t("preflight.formatMp3")}</option><option value="m4a">{t("preflight.formatM4a")}</option><option value="wav">{t("preflight.formatWav")}</option></Select></Field>
          <Field label={t("preflight.bitrate")}><Select value={bitrateKbps} onChange={(event) => setBitrateKbps(Number(event.target.value))}>{[96, 128, 160, 192, 256, 320].map((value) => <option key={value} value={value}>{t("preflight.bitrateValue", { value })}</option>)}</Select></Field>
          <SwitchField checked={splitPerChapter} onCheckedChange={setSplitPerChapter} label={t("preflight.splitPerChapter")} detail={t("preflight.singleFileDetail")} />
          <SwitchField checked={strictRetailer} onCheckedChange={setStrictRetailer} label={t("proofing.strictRetailer")} detail={t("proofing.strictRetailerDetail")} />
        </div>
        {dirtySegmentIds.size ? <Card className="inline-notice"><AlertTriangle size={18} /><div><strong>{t("proofing.unsavedEdits")}</strong><p>{t("proofing.unsavedEditsDetail", { count: dirtySegmentIds.size })}</p></div></Card> : null}
        {startExport.isError ? <ErrorState error={startExport.error} onRetry={() => startExport.mutate()} /> : null}
        {startExport.data ? <Card className="proofing-job-created"><Check size={18} /><div><strong>{t("proofing.exportStarted")}</strong><p>{t("proofing.exportStartedDetail")}</p></div><Link className="button button-secondary button-sm" to={`/jobs/${startExport.data.id}`}>{t("proofing.openJob")}</Link></Card> : null}
        <div className="panel-footer"><Button disabled={!exportReady || dirtySegmentIds.size > 0 || startExport.isPending} onClick={() => startExport.mutate()}>{startExport.isPending ? <LoaderCircle className="spin" size={16} /> : <FileAudio size={16} />}{t("proofing.startReExport")}</Button></div>
      </Card>
    </div>
  );
}

function ProofingSegmentCard({
  projectId,
  view,
  speakerName,
  performanceDescriptor,
  directionLookupPending,
  onChanged,
  onDirtyChange,
}: {
  projectId: string;
  view: ProofingSegmentView;
  speakerName: string;
  performanceDescriptor?: ModelPerformanceCapabilities;
  directionLookupPending: boolean;
  onChanged: () => Promise<void>;
  onDirtyChange: (segmentId: string, dirty: boolean) => void;
}) {
  const { t, i18n } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const [showTakes, setShowTakes] = useState(false);
  const [textOverride, setTextOverride] = useState(view.segment.narration_text_override ?? "");
  const [performance, setPerformance] = useState<PerformanceSettings>(view.segment.performance_override);
  const [timing, setTiming] = useState<TimingSettings>(view.segment.timing_override);
  const [draftBase, setDraftBase] = useState(() => segmentDraftBase(view.segment));
  const [draftStale, setDraftStale] = useState(false);
  const highestObservedRevision = useRef(view.segment.revision);
  highestObservedRevision.current = Math.max(highestObservedRevision.current, view.segment.revision);
  const [regenerationConfirmed, setRegenerationConfirmed] = useState(false);
  const [allowBudgetOverride, setAllowBudgetOverride] = useState(false);
  const [reestimateAfterRefresh, setReestimateAfterRefresh] = useState(false);
  const hasUnsavedChanges = textOverride !== draftBase.textOverride
    || !settingsEqual(performance, draftBase.performance)
    || !settingsEqual(timing, draftBase.timing);
  const adoptSegment = useCallback((segment: ProductionSegment): boolean => {
    if (segment.revision < highestObservedRevision.current || segment.revision < draftBase.revision) return false;
    highestObservedRevision.current = segment.revision;
    setTextOverride(segment.narration_text_override ?? "");
    setPerformance(segment.performance_override);
    setTiming(segment.timing_override);
    setDraftBase(segmentDraftBase(segment));
    setDraftStale(false);
    return true;
  }, [draftBase.revision]);

  useEffect(() => {
    if (view.segment.revision <= draftBase.revision) return;
    if (hasUnsavedChanges) setDraftStale(true);
    else adoptSegment(view.segment);
  }, [adoptSegment, draftBase.revision, hasUnsavedChanges, view.segment]);

  useEffect(() => {
    onDirtyChange(view.segment.id, hasUnsavedChanges);
  }, [hasUnsavedChanges, onDirtyChange, view.segment.id]);

  useEffect(() => () => onDirtyChange(view.segment.id, false), [onDirtyChange, view.segment.id]);

  const takes = useQuery({ queryKey: ["proofing", projectId, "segments", view.segment.id, "takes"], queryFn: () => api.proofingTakes(projectId, view.segment.id), enabled: showTakes });
  const save = useMutation({
    mutationFn: () => api.updateProofingSegment(projectId, view.segment.id, {
      expectedRevision: draftBase.revision,
      textOverride: textOverride.trim() || undefined,
      clearTextOverride: !textOverride.trim(),
      // Capability discovery is advisory for the editor. Preserve the durable draft when
      // discovery is still loading, stale, or temporarily unavailable; unsupported values
      // are removed only through the explicit "clear unsupported direction" action below.
      performanceOverride: performance,
      timingOverride: timing,
    }),
    onSuccess: async (data) => {
      adoptSegment(data.segment);
      estimate.reset();
      regenerate.reset();
      setRegenerationConfirmed(false);
      await onChanged();
    },
    onError: (error) => {
      if (error instanceof ApiError && error.problem.code === "stale_segment_revision") {
        setDraftStale(true);
        void onChanged();
      }
    },
  });
  const review = useMutation({ mutationFn: (state: SegmentReviewState) => api.updateProofingReview(projectId, view.segment.id, state, view.segment.revision), onSuccess: onChanged });
  const selectTake = useMutation({
    mutationFn: (takeId: string) => api.selectProofingTake(projectId, view.segment.id, {
      takeId,
      expectedRevision: view.selection?.revision ?? 0,
      expectedSegmentRevision: view.segment.revision,
    }),
    onSuccess: async () => { await onChanged(); },
  });
  const estimate = useMutation({
    mutationFn: () => api.proofingRegenerationEstimate(projectId, view.segment.id, view.segment.revision),
    onSuccess: () => {
      regenerate.reset();
      setRegenerationConfirmed(false);
    },
  });
  const regenerate = useMutation({
    mutationFn: () => {
      if (!estimate.data) throw new Error(t("proofing.estimateRequired"));
      return api.startProofingRegeneration(projectId, view.segment.id, view.segment.revision, estimate.data.estimateToken, allowBudgetOverride);
    },
    onSuccess: async () => { await onChanged(); },
    onSettled: () => setRegenerationConfirmed(false),
  });
  const recoverEstimate = async () => {
    estimate.reset();
    regenerate.reset();
    setRegenerationConfirmed(false);
    await onChanged();
    setReestimateAfterRefresh(true);
  };

  useEffect(() => {
    if (!reestimateAfterRefresh) return;
    setReestimateAfterRefresh(false);
    estimate.mutate();
  }, [reestimateAfterRefresh]);

  const startRegeneration = () => {
    if (!estimate.data || Date.parse(estimate.data.expiresAt) <= Date.now()) {
      void recoverEstimate();
      return;
    }
    regenerate.mutate();
  };
  const locked = view.segment.review_state === "locked";
  const selectedHasFindings = Boolean(view.selectedTake?.findings.length);
  const performanceCapability = performanceDescriptor?.performance;
  const unsupportedPerformance = hasUnsupportedPerformance(performance, performanceDescriptor);

  return (
    <Card className={clsx("proofing-segment", !view.selectedTakeCurrent && "proofing-segment-stale", view.segment.review_state === "flagged" && "proofing-segment-flagged")}>
      <div className="proofing-segment-summary">
        <button type="button" className="proofing-segment-toggle" aria-expanded={expanded} onClick={() => setExpanded((value) => !value)}>
          <span className="proofing-ordinal">{String(view.segment.ordinal + 1).padStart(3, "0")}</span>
          <span className="proofing-segment-copy"><span className="cluster"><strong>{speakerName}</strong><Badge tone={reviewTone(view.segment.review_state)}>{t(`proofing.state.${view.segment.review_state}`)}</Badge>{!view.selectedTake ? <Badge tone="danger">{t("proofing.missing")}</Badge> : !view.selectedTakeCurrent ? <Badge tone="warning">{t("proofing.stale")}</Badge> : <Badge tone="positive">{t("proofing.current")}</Badge>}{selectedHasFindings ? <Badge tone="warning">{t("proofing.findingCount", { count: view.selectedTake?.findings.length })}</Badge> : null}</span><span>{view.segment.effective_text}</span></span>
          <span className="proofing-take-count"><Headphones size={15} />{t("proofing.takeCount", { count: view.takeCount })}</span>
        </button>
        {view.audioUrl ? <audio aria-label={t("proofing.selectedAudioLabel", { speaker: speakerName })} className="proofing-selected-audio" controls preload="none" src={view.audioUrl}>{t("auditions.audioUnsupported")}</audio> : null}
      </div>

      {expanded ? <div className="proofing-segment-detail stack">
        <div className="proofing-review-actions" role="group" aria-label={t("proofing.reviewActions")}>
          <Button size="sm" variant={view.segment.review_state === "flagged" ? "primary" : "secondary"} disabled={locked || review.isPending || hasUnsavedChanges} onClick={() => review.mutate("flagged")}><Flag size={14} />{t("proofing.flag")}</Button>
          <Button size="sm" variant={view.segment.review_state === "approved" ? "primary" : "secondary"} disabled={review.isPending || hasUnsavedChanges || !view.selectedTakeCurrent} onClick={() => review.mutate("approved")}><Check size={14} />{locked ? t("proofing.unlock") : t("proofing.approveAction")}</Button>
          <Button size="sm" variant={view.segment.review_state === "locked" ? "primary" : "secondary"} disabled={locked || review.isPending || hasUnsavedChanges || !view.selectedTakeCurrent} onClick={() => review.mutate("locked")}><Lock size={14} />{t("proofing.lock")}</Button>
        </div>
        {review.isError ? <ErrorState error={review.error} onRetry={() => review.mutate(review.variables ?? view.segment.review_state)} /> : null}

        <div className="proofing-editor-grid">
          <Field label={t("proofing.narrationText")} hint={t("proofing.narrationTextHint")} className="proofing-text-field"><Textarea aria-label={t("proofing.narrationText")} disabled={locked} value={textOverride} onChange={(event) => { setTextOverride(event.target.value); estimate.reset(); }} placeholder={view.segment.original_text} /></Field>
          <Card className="proofing-original"><strong>{t("proofing.originalText")}</strong><p>{view.segment.original_text}</p></Card>
        </div>
        <details className="proofing-direction" open>
          <summary>{t("proofing.performanceAndTiming")}</summary>
          {directionLookupPending ? <p className="audition-capability-note"><LoaderCircle className="spin" size={14} />{t("proofing.directionLoading")}</p> : performanceDescriptor ? <p className="audition-capability-note supported"><Check size={14} />{t("proofing.directionVerified", { model: performanceDescriptor.model })}</p> : <p className="audition-capability-note"><AlertTriangle size={14} />{t("proofing.directionUnavailable")}</p>}
          {unsupportedPerformance ? <Card className="inline-notice"><AlertTriangle size={16} /><div><strong>{t("proofing.unsupportedDirection")}</strong><p>{t("proofing.unsupportedDirectionDetail")}</p></div><Button size="sm" variant="secondary" disabled={locked} onClick={() => { setPerformance(supportedPerformance(performance, performanceDescriptor)); estimate.reset(); }}>{t("proofing.clearUnsupportedDirection")}</Button></Card> : null}
          <div className="performance-grid">
            <Field label={t("proofing.speed")} hint={rangeHint(performanceCapability?.speed)}><Input aria-label={t("proofing.speed")} disabled={locked || !performanceCapability?.speed} type="number" min={performanceCapability?.speed?.minimum} max={performanceCapability?.speed?.maximum} step="0.05" value={performance.speed ?? ""} onChange={(event) => { setPerformance({ ...performance, speed: optionalNumber(event.target.value) }); estimate.reset(); }} /></Field>
            <Field label={t("proofing.pitch")} hint={rangeHint(performanceCapability?.pitch)}><Input aria-label={t("proofing.pitch")} disabled={locked || !performanceCapability?.pitch} type="number" min={performanceCapability?.pitch?.minimum} max={performanceCapability?.pitch?.maximum} step="0.05" value={performance.pitch ?? ""} onChange={(event) => { setPerformance({ ...performance, pitch: optionalNumber(event.target.value) }); estimate.reset(); }} /></Field>
            <Field label={t("proofing.stability")} hint={rangeHint(performanceCapability?.stability)}><Input aria-label={t("proofing.stability")} disabled={locked || !performanceCapability?.stability} type="number" min={performanceCapability?.stability?.minimum} max={performanceCapability?.stability?.maximum} step="0.05" value={performance.stability ?? ""} onChange={(event) => { setPerformance({ ...performance, stability: optionalNumber(event.target.value) }); estimate.reset(); }} /></Field>
            <Field label={t("proofing.similarity")} hint={rangeHint(performanceCapability?.similarity)}><Input aria-label={t("proofing.similarity")} disabled={locked || !performanceCapability?.similarity} type="number" min={performanceCapability?.similarity?.minimum} max={performanceCapability?.similarity?.maximum} step="0.05" value={performance.similarity ?? ""} onChange={(event) => { setPerformance({ ...performance, similarity: optionalNumber(event.target.value) }); estimate.reset(); }} /></Field>
            <Field label={t("proofing.style")} hint={rangeHint(performanceCapability?.style)}><Input aria-label={t("proofing.style")} disabled={locked || !performanceCapability?.style} type="number" min={performanceCapability?.style?.minimum} max={performanceCapability?.style?.maximum} step="0.05" value={performance.style ?? ""} onChange={(event) => { setPerformance({ ...performance, style: optionalNumber(event.target.value) }); estimate.reset(); }} /></Field>
            <Field label={t("proofing.deliveryCue")}><Select aria-label={t("proofing.deliveryCue")} disabled={locked || !performanceCapability?.delivery_cues.length} value={performance.delivery_cue ?? ""} onChange={(event) => { setPerformance({ ...performance, delivery_cue: (event.target.value || undefined) as DeliveryCue | undefined }); estimate.reset(); }}><option value="">{t("proofing.inherit")}</option>{(performanceCapability?.delivery_cues ?? []).map((cue) => <option key={cue} value={cue}>{t(`proofing.cue.${cue}`)}</option>)}</Select></Field>
            <Field label={t("proofing.speakerBoost")}><Select aria-label={t("proofing.speakerBoost")} disabled={locked || !performanceCapability?.speaker_boost} value={performance.speaker_boost == null ? "" : String(performance.speaker_boost)} onChange={(event) => { setPerformance({ ...performance, speaker_boost: event.target.value === "" ? undefined : event.target.value === "true" }); estimate.reset(); }}><option value="">{t("proofing.inherit")}</option><option value="true">{t("common.on")}</option><option value="false">{t("common.off")}</option></Select></Field>
            <Field label={t("proofing.pauseBefore")} hint="0–5000 ms"><Input aria-label={t("proofing.pauseBefore")} disabled={locked} type="number" min="0" max="5000" step="50" value={timing.pause_before_ms ?? ""} onChange={(event) => { setTiming({ ...timing, pause_before_ms: optionalNumber(event.target.value) }); estimate.reset(); }} /></Field>
            <Field label={t("proofing.pauseAfter")} hint="0–5000 ms"><Input aria-label={t("proofing.pauseAfter")} disabled={locked} type="number" min="0" max="5000" step="50" value={timing.pause_after_ms ?? ""} onChange={(event) => { setTiming({ ...timing, pause_after_ms: optionalNumber(event.target.value) }); estimate.reset(); }} /></Field>
          </div>
        </details>
        {draftStale ? <Card className="inline-notice" role="alert"><AlertTriangle size={18} /><div><strong>{t("proofing.segmentConflict")}</strong><p>{t("proofing.segmentConflictHint")}</p></div><Button size="sm" variant="secondary" onClick={() => { adoptSegment(view.segment); estimate.reset(); regenerate.reset(); setRegenerationConfirmed(false); void onChanged(); }}>{t("proofing.reloadSegment")}</Button></Card> : null}
        {save.isError ? <ErrorState error={save.error} onRetry={save.error instanceof ApiError && save.error.problem.code === "stale_segment_revision" ? undefined : () => save.mutate()} /> : null}
        {hasUnsavedChanges ? <p className="proofing-dirty-note"><AlertTriangle size={14} />{t("proofing.saveBeforeEstimate")}</p> : null}
        <div className="panel-footer"><Button disabled={locked || !hasUnsavedChanges || save.isPending} onClick={() => save.mutate()}>{save.isPending ? <LoaderCircle className="spin" size={15} /> : <Save size={15} />}{t("proofing.saveOverrides")}</Button></div>

        <div className="proofing-takes-section stack">
          <div className="space-between"><div><h3>{t("proofing.takes")}</h3><p>{t("proofing.takesDetail")}</p></div><Button size="sm" variant="secondary" onClick={() => setShowTakes((value) => !value)}><Volume2 size={15} />{showTakes ? t("proofing.hideTakes") : t("proofing.showTakes")}</Button></div>
          {showTakes && takes.isLoading ? <LoadingState label={t("proofing.loadingTakes")} /> : null}
          {takes.isError ? <ErrorState error={takes.error} onRetry={() => void takes.refetch()} /> : null}
          {showTakes && takes.data ? <div className="proofing-takes">{takes.data.map((take) => {
            const selected = view.selection?.take_id === take.id;
            const current = take.semantic_input_hash === view.segment.expected_input_hash;
            return <Card className={clsx("proofing-take", selected && "selected")} key={take.id}><div className="space-between"><div className="cluster"><strong>{t("proofing.takeOrdinal", { count: take.ordinal + 1 })}</strong>{selected ? <Badge tone="accent">{t("proofing.selected")}</Badge> : null}{current ? <Badge tone="positive">{t("proofing.current")}</Badge> : <Badge tone="warning">{t("proofing.stale")}</Badge>}</div><span>{(take.duration_ms / 1000).toFixed(1)} s</span></div><audio aria-label={t("proofing.takeAudioLabel", { count: take.ordinal + 1, speaker: speakerName })} controls preload="none" src={artifactUrl(take.artifact_id)}>{t("auditions.audioUnsupported")}</audio>{take.findings.length ? <ul className="proofing-findings">{take.findings.map((finding) => <li key={`${finding.code}-${finding.start_ms ?? 0}`} className={finding.severity}><AlertTriangle size={14} /><span><strong>{finding.code}</strong>{finding.message}</span></li>)}</ul> : <p className="proofing-clean"><ShieldCheck size={15} />{t("proofing.noFindings")}</p>}<Button size="sm" variant={selected ? "ghost" : "secondary"} disabled={selected || locked || hasUnsavedChanges || selectTake.isPending} onClick={() => selectTake.mutate(take.id)}>{selected ? <Check size={14} /> : <Play size={14} />}{selected ? t("proofing.selected") : t("proofing.selectTake")}</Button></Card>;
          })}</div> : null}
          {selectTake.isError ? <ErrorState error={selectTake.error} /> : null}
        </div>

        <Card className="proofing-regeneration stack">
          <div className="space-between"><div><h3>{t("proofing.regenerate")}</h3><p>{t("proofing.regenerateDetail")}</p></div>{estimate.data ? <Badge tone={estimate.data.unknownPricing ? "warning" : "accent"}>{estimate.data.unknownPricing ? t("proofing.unknownPrice") : formatMoney(estimate.data.monetaryCostMicros ?? undefined, estimate.data.currency ?? undefined, i18n.language)}</Badge> : null}</div>
          {!estimate.data ? <Button variant="secondary" disabled={locked || hasUnsavedChanges || estimate.isPending || reestimateAfterRefresh} onClick={() => estimate.mutate()}>{estimate.isPending || reestimateAfterRefresh ? <LoaderCircle className="spin" size={15} /> : <Sparkles size={15} />}{t("proofing.getEstimate")}</Button> : <div className="regeneration-quote"><div className="proofing-quote-grid"><Stat label={t("proofing.provider")} value={estimate.data.providerName} detail={estimate.data.model} /><Stat label={t("proofing.characters")} value={formatCount(estimate.data.characters, i18n.language)} /><Stat label={t("proofing.cost")} value={estimate.data.unknownPricing ? t("proofing.unknownPrice") : formatMoney(estimate.data.monetaryCostMicros ?? undefined, estimate.data.currency ?? undefined, i18n.language)} detail={estimate.data.credits != null ? t("proofing.creditCount", { count: estimate.data.credits }) : undefined} /><Stat label={t("proofing.expires")} value={formatDate(estimate.data.expiresAt, i18n.language)} /></div><SwitchField checked={regenerationConfirmed} disabled={regenerate.isPending} onCheckedChange={setRegenerationConfirmed} label={t("proofing.confirmRegeneration")} detail={t("proofing.confirmRegenerationDetail")} /><SwitchField checked={allowBudgetOverride} disabled={regenerate.isPending} onCheckedChange={setAllowBudgetOverride} label={t("proofing.allowBudgetOverride")} detail={t("proofing.allowBudgetOverrideDetail")} /><div className="cluster"><Button variant="secondary" disabled={hasUnsavedChanges || estimate.isPending || regenerate.isPending} onClick={() => estimate.mutate()}><RefreshCw size={14} />{t("proofing.refreshEstimate")}</Button><Button disabled={hasUnsavedChanges || !regenerationConfirmed || regenerate.isPending || needsFreshEstimate(regenerate.error)} onClick={startRegeneration}>{regenerate.isPending ? <LoaderCircle className="spin" size={15} /> : <Play size={15} />}{t("proofing.startRegeneration")}</Button></div></div>}
          {estimate.isError ? <ErrorState error={estimate.error} onRetry={needsFreshEstimate(estimate.error) ? () => void recoverEstimate() : () => estimate.mutate()} /> : null}
          {regenerate.isError ? <ErrorState error={regenerate.error} onRetry={needsFreshEstimate(regenerate.error) ? () => void recoverEstimate() : undefined} /> : null}
          {regenerate.data ? <Card className="proofing-job-created"><Clock3 size={18} /><div><strong>{t("proofing.regenerationStarted")}</strong><p>{t("proofing.regenerationStartedDetail")}</p></div><Link className="button button-secondary button-sm" to={`/jobs/${regenerate.data.id}`}>{t("proofing.openJob")}</Link></Card> : null}
        </Card>
      </div> : null}
    </Card>
  );
}
