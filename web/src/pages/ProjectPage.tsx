import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { clsx } from "clsx";
import {
  AlertTriangle,
  ArrowLeft,
  BookOpen,
  Check,
  CheckCircle2,
  CircleAlert,
  Clock3,
  FlaskConical,
  Gauge,
  Headphones,
  LoaderCircle,
  MessageSquareQuote,
  Mic2,
  PackageCheck,
  Pencil,
  Play,
  Plus,
  Save,
  ShieldCheck,
  Sparkles,
  Trash2,
  Upload,
  UserRound,
  Volume2,
  XCircle,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link, useNavigate, useParams } from "react-router-dom";
import { ApiError, api, jobEventsUrl } from "../api/client";
import type { Character, CharacterDetectionInput, DetectionReasoning, DetectionTemperature, DialogueEvidence, ExportFormat, PronunciationRule, ProviderProfile, Voice, VoiceAssignment } from "../api/types";
import { ErrorState, EmptyState, LoadingState } from "../components/StateViews";
import { Badge, Button, Card, Dialog, Field, Input, PageHeading, ProgressBar, Select, Stat, SwitchField, Textarea } from "../components/ui";
import { DEFAULT_EXPORT_SETTINGS, requiresMusicOwnership, toJobExportSettings, type ExportFormState } from "../features/exportSettings";
import { formatBytes, formatCount, formatDuration, formatMoney } from "../lib/format";
import { AUTO_SPEAKER, NARRATOR_SPEAKER, paragraphIdFor, parseAliases, speakerOverrideInput, storedSpeakerSelection } from "../features/characterReview";
import { DistributionPanel } from "../features/DistributionPanel";
import { ProofingWorkbench } from "../features/ProofingWorkbench";
import { VoiceAuditionPanel } from "../features/VoiceAuditionPanel";

type ProjectTab = "chapters" | "characters" | "auditions" | "pronunciation" | "preflight" | "proofing" | "distribution";

const tabs: { id: ProjectTab; label: string; icon: typeof BookOpen }[] = [
  { id: "chapters", label: "project.chapters", icon: BookOpen },
  { id: "characters", label: "project.characters", icon: UserRound },
  { id: "auditions", label: "project.auditions", icon: Headphones },
  { id: "pronunciation", label: "project.pronunciation", icon: Volume2 },
  { id: "preflight", label: "project.preflight", icon: Gauge },
  { id: "proofing", label: "project.proofing", icon: ShieldCheck },
  { id: "distribution", label: "project.distribution", icon: PackageCheck },
];

function retryIdempotencyKey(error: unknown, previous?: string): string {
  return error instanceof ApiError && error.problem.status === 0 && previous
    ? previous
    : crypto.randomUUID();
}

export function ProjectPage({ tab }: { tab: ProjectTab }) {
  const { id = "" } = useParams();
  const { t } = useTranslation();
  const project = useQuery({ queryKey: ["project", id], queryFn: () => api.project(id), enabled: Boolean(id) });

  if (project.isLoading) return <LoadingState label={t("state.loadingProject")} />;
  if (project.isError) return <ErrorState error={project.error} onRetry={() => void project.refetch()} />;
  if (!project.data) return null;

  return (
    <div className="page project-page">
      <Link className="back-link" to="/library"><ArrowLeft size={16} />{t("nav.library")}</Link>
      <div className="project-header">
        <div className="project-mini-cover">{project.data.coverUrl ? <img src={project.data.coverUrl} alt="" /> : <BookOpen size={24} />}</div>
        <div>
          <div className="cluster"><Badge tone={project.data.status === "completed" ? "positive" : project.data.status === "failed" ? "danger" : "accent"}>{t(`library.${project.data.status}`)}</Badge>{project.data.series ? <span className="project-series">{project.data.series}{project.data.seriesPosition ? ` · ${project.data.seriesPosition}` : ""}</span> : null}</div>
          <h1>{project.data.title}</h1>
          <p>{project.data.author || t("common.unknown")} · {t("library.chapters", { count: project.data.chapterCount })}</p>
        </div>
      </div>
      <nav className="project-tabs" aria-label={project.data.title}>
        {tabs.map((item) => {
          const Icon = item.icon;
          return <Link key={item.id} className={clsx("project-tab", tab === item.id && "active")} aria-current={tab === item.id ? "page" : undefined} to={`/projects/${id}/${item.id}`}><Icon size={16} />{t(item.label)}</Link>;
        })}
      </nav>

      <div className="project-tab-content">
        {tab === "chapters" ? <ChaptersPanel projectId={id} /> : null}
        {tab === "characters" ? <CharactersPanel projectId={id} reviewStatus={project.data.characterReviewStatus} consentCloudAudio={project.data.consentCloudAudio} /> : null}
        {tab === "auditions" ? <VoiceAuditionPanel projectId={id} /> : null}
        {tab === "pronunciation" ? <PronunciationPanel projectId={id} defaultLanguage={project.data.language ?? ""} /> : null}
        {tab === "preflight" ? <PreflightPanel projectId={id} /> : null}
        {tab === "proofing" ? <ProofingWorkbench projectId={id} /> : null}
        {tab === "distribution" ? <DistributionPanel projectId={id} /> : null}
      </div>
    </div>
  );
}

function ChaptersPanel({ projectId }: { projectId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const project = useQuery({ queryKey: ["project", projectId], queryFn: () => api.project(projectId) });
  const [selection, setSelection] = useState<Record<string, boolean>>();
  const values = selection ?? Object.fromEntries(project.data?.chapters.map((chapter) => [chapter.id, chapter.selected]) ?? []);
  const save = useMutation({
    mutationFn: () => api.updateProject(projectId, {
      chapters: project.data?.chapters.map((chapter) => ({ ...chapter, selected: values[chapter.id] ?? false })) ?? [],
    }),
    onSuccess: (data) => { queryClient.setQueryData(["project", projectId], data); setSelection(undefined); },
  });

  if (project.isLoading) return <LoadingState label={t("state.loadingProject")} />;
  if (project.isError) return <ErrorState error={project.error} onRetry={() => void project.refetch()} />;
  if (!project.data?.chapters.length) return <EmptyState title={t("project.noChapters")} detail={t("project.selectHint")} />;
  const selectedCount = Object.values(values).filter(Boolean).length;

  return (
    <div>
      <div className="section-heading chapter-heading">
        <div><h2>{t("project.chapters")}</h2><p>{t("project.selectHint")}</p></div>
        <div className="cluster"><Badge tone="accent">{t("import.selectedCount", { selected: selectedCount, total: project.data.chapters.length })}</Badge><Button onClick={() => save.mutate()} disabled={save.isPending || !selection}><Save size={16} />{save.isPending ? t("state.saving") : t("project.saveSelection")}</Button></div>
      </div>
      {save.isError ? <ErrorState error={save.error} onRetry={() => save.mutate()} /> : null}
      <div className="chapter-list">
        {project.data.chapters.map((chapter) => (
          <label className={clsx("chapter-row", values[chapter.id] && "selected")} key={chapter.id}>
            <input type="checkbox" checked={Boolean(values[chapter.id])} onChange={(event) => setSelection({ ...values, [chapter.id]: event.target.checked })} aria-label={t("project.chapterSelected", { title: chapter.title })} />
            <span className="chapter-number">{String(chapter.index + 1).padStart(2, "0")}</span>
            <span className="chapter-main"><strong>{chapter.title}</strong><small>{t("project.words", { count: chapter.wordCount })} · {t("project.charactersCount", { count: chapter.characterCount })}</small></span>
            <span className="chapter-duration">{chapter.estimatedSeconds ? formatDuration(chapter.estimatedSeconds) : "—"}</span>
            <Badge tone={chapter.status === "complete" || chapter.status === "cached" ? "positive" : chapter.status === "failed" ? "danger" : chapter.status === "processing" ? "accent" : "neutral"}>{t(`project.${chapter.status}`)}</Badge>
          </label>
        ))}
      </div>
      <div className="panel-footer"><Link className="button button-primary button-md" to={`/projects/${projectId}/characters`}>{t("common.continue")}<UserRound size={16} /></Link></div>
    </div>
  );
}

export function characterDetectionInput(
  providerProfileId: string,
  temperatureMode: DetectionTemperature["mode"],
  temperatureValue: number,
  reasoningMode: DetectionReasoning["mode"],
  reasoningEffort: "minimal" | "low" | "medium" | "high",
  reasoningTokens: number,
  expectedCharacterRevision: number,
): CharacterDetectionInput {
  const temperature: DetectionTemperature = temperatureMode === "value"
    ? { mode: "value", value: temperatureValue }
    : { mode: temperatureMode };
  let reasoning: DetectionReasoning;
  if (reasoningMode === "effort") reasoning = { mode: "effort", effort: reasoningEffort };
  else if (reasoningMode === "token_budget") reasoning = { mode: "token_budget", tokens: reasoningTokens };
  else reasoning = { mode: reasoningMode };
  return { providerProfileId, temperature, reasoning, expectedCharacterRevision };
}

function CharactersPanel({ projectId, reviewStatus, consentCloudAudio }: { projectId: string; reviewStatus: string; consentCloudAudio: boolean }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const characters = useQuery({ queryKey: ["characters", projectId], queryFn: () => api.characters(projectId) });
  const detectionStatus = useQuery({
    queryKey: ["character-detection", projectId],
    queryFn: () => api.characterDetectionStatus(projectId),
    refetchInterval: (query) => query.state.data?.activeJob ? 3_000 : false,
  });
  const providers = useQuery({ queryKey: ["providers"], queryFn: api.providers });
  const voices = useQuery({ queryKey: ["voices"], queryFn: () => api.voices() });
  const aiProviders = providers.data?.items.filter((provider) => provider.capabilities?.characterDetection) ?? [];
  const [detectionProvider, setDetectionProvider] = useState("");
  const [temperatureMode, setTemperatureMode] = useState<DetectionTemperature["mode"]>("default");
  const [temperatureValue, setTemperatureValue] = useState(0.2);
  const [reasoningMode, setReasoningMode] = useState<DetectionReasoning["mode"]>("inherit");
  const [reasoningEffort, setReasoningEffort] = useState<"minimal" | "low" | "medium" | "high">("medium");
  const [reasoningTokens, setReasoningTokens] = useState(4096);
  const [assigning, setAssigning] = useState<Character>();
  const [editingIdentity, setEditingIdentity] = useState<Character>();
  const [identityName, setIdentityName] = useState("");
  const [identityAliases, setIdentityAliases] = useState("");
  const [voiceProvider, setVoiceProvider] = useState("");
  const [voiceId, setVoiceId] = useState("");
  const [speakerSelections, setSpeakerSelections] = useState<Record<string, { selection: string; characterRevision: number }>>({});
  const [voiceLibraryOpen, setVoiceLibraryOpen] = useState(false);
  const [addingCharacter, setAddingCharacter] = useState(false);
  const [newCharacterName, setNewCharacterName] = useState("");
  const [newCharacterAliases, setNewCharacterAliases] = useState("");
  const [mergingCharacter, setMergingCharacter] = useState<Character>();
  const [mergeTargetId, setMergeTargetId] = useState("");
  const [mergeConfirmed, setMergeConfirmed] = useState(false);
  const [deletingCharacter, setDeletingCharacter] = useState<Character>();
  const [deleteCharacterConfirmed, setDeleteCharacterConfirmed] = useState(false);
  const characterRevision = characters.data?.characterRevision ?? 0;
  const activeDetection = detectionStatus.data?.activeJob;
  const latestDetection = detectionStatus.data?.latestJob;
  const detection = useMutation({
    mutationFn: (idempotencyKey: string) => api.detectCharacters(projectId, characterDetectionInput(
      detectionProvider,
      temperatureMode,
      temperatureValue,
      reasoningMode,
      reasoningEffort,
      reasoningTokens,
      characterRevision,
    ), idempotencyKey),
    onSuccess: async (job) => {
      queryClient.setQueryData(["character-detection", projectId], { activeJob: job, latestJob: job });
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["jobs"] }),
        queryClient.invalidateQueries({ queryKey: ["project", projectId] }),
        queryClient.invalidateQueries({ queryKey: ["characters", projectId] }),
      ]);
    },
  });
  const approve = useMutation({ mutationFn: () => api.approveCharacters(projectId, characterRevision), onSuccess: async () => { await Promise.all([queryClient.invalidateQueries({ queryKey: ["project", projectId] }), queryClient.invalidateQueries({ queryKey: ["characters", projectId] })]); } });
  const assignment = useMutation({
    mutationFn: async () => {
      const provider = providers.data?.items.find((item) => item.id === voiceProvider);
      const voice = voices.data?.items.find((item) => item.id === voiceId);
      if (!assigning || !provider || !voice) throw new Error(t("characters.missingVoiceSelection"));
      const value: VoiceAssignment = {
        providerProfileId: provider.id,
        providerName: provider.name,
        voiceId: voice.id,
        voiceName: voice.name,
        model: provider.model,
        performance: assigning.voiceAssignment?.performance ?? {},
        timing: assigning.voiceAssignment?.timing ?? {},
      };
      return api.assignVoice(projectId, assigning.id, value, characterRevision);
    },
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["characters", projectId] }),
        queryClient.invalidateQueries({ queryKey: ["project", projectId] }),
      ]);
      setAssigning(undefined);
    },
  });
  const identity = useMutation({
    mutationFn: ({ characterId, name, aliases }: { characterId: string; name: string; aliases: string[] }) =>
      api.updateCharacter(projectId, characterId, { canonicalName: name, aliases, expectedCharacterRevision: characterRevision }),
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["characters", projectId] }),
        queryClient.invalidateQueries({ queryKey: ["project", projectId] }),
      ]);
      setEditingIdentity(undefined);
    },
  });
  const speakerOverride = useMutation({
    mutationFn: ({ evidence, selection }: { evidence: DialogueEvidence; selection: string }) =>
      selection === AUTO_SPEAKER
        ? api.deleteSpeakerOverride(projectId, paragraphIdFor(evidence), evidence.startOffset, evidence.endOffset, characterRevision)
        : api.setSpeakerOverride(projectId, paragraphIdFor(evidence), speakerOverrideInput(evidence, selection), characterRevision),
    onSuccess: async (data, variables) => {
      const evidenceKey = `${paragraphIdFor(variables.evidence)}:${variables.evidence.startOffset}:${variables.evidence.endOffset}`;
      setSpeakerSelections((current) => ({
        ...current,
        [evidenceKey]: { selection: variables.selection, characterRevision: data.characterRevision },
      }));
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["characters", projectId] }),
        queryClient.invalidateQueries({ queryKey: ["project", projectId] }),
      ]);
    },
  });
  const createIdentity = useMutation({
    mutationFn: (idempotencyKey: string) => api.createCharacter(projectId, {
      canonicalName: newCharacterName.trim(),
      aliases: parseAliases(newCharacterAliases, newCharacterName),
      expectedCharacterRevision: characterRevision,
    }, idempotencyKey),
    onSuccess: async () => {
      await Promise.all([queryClient.invalidateQueries({ queryKey: ["characters", projectId] }), queryClient.invalidateQueries({ queryKey: ["project", projectId] })]);
      setAddingCharacter(false); setNewCharacterName(""); setNewCharacterAliases("");
    },
  });
  const mergeIdentity = useMutation({
    mutationFn: (idempotencyKey: string) => {
      if (!mergingCharacter || !mergeTargetId) throw new Error(t("characters.missingMergeTarget"));
      return api.mergeCharacter(projectId, mergingCharacter.id, mergeTargetId, characterRevision, idempotencyKey);
    },
    onSuccess: async () => {
      await Promise.all([queryClient.invalidateQueries({ queryKey: ["characters", projectId] }), queryClient.invalidateQueries({ queryKey: ["project", projectId] }), queryClient.invalidateQueries({ queryKey: ["pronunciation-rules"] })]);
      setMergingCharacter(undefined); setMergeTargetId(""); setMergeConfirmed(false);
    },
  });
  const deleteIdentity = useMutation({
    mutationFn: (idempotencyKey: string) => {
      if (!deletingCharacter) throw new Error(t("characters.missingCharacter"));
      return api.deleteCharacter(projectId, deletingCharacter.id, characterRevision, idempotencyKey);
    },
    onSuccess: async () => {
      await Promise.all([queryClient.invalidateQueries({ queryKey: ["characters", projectId] }), queryClient.invalidateQueries({ queryKey: ["project", projectId] })]);
      setDeletingCharacter(undefined); setDeleteCharacterConfirmed(false);
    },
  });
  const detectionAction = useMutation({
    mutationFn: (action: "pause" | "resume" | "cancel" | "retry") => {
      const job = detectionStatus.data?.activeJob ?? detectionStatus.data?.latestJob;
      if (!job) throw new Error(t("characters.missingDetectionJob"));
      return api.jobAction(job.id, action);
    },
    onSuccess: async (job) => {
      queryClient.setQueryData(["job", job.id], job);
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["character-detection", projectId] }),
        queryClient.invalidateQueries({ queryKey: ["jobs"] }),
      ]);
    },
  });
  useEffect(() => {
    if (!activeDetection || typeof EventSource === "undefined") return;
    const source = new EventSource(jobEventsUrl(activeDetection.id), { withCredentials: true });
    const refresh = () => {
      void queryClient.invalidateQueries({ queryKey: ["character-detection", projectId] });
      void queryClient.invalidateQueries({ queryKey: ["job", activeDetection.id] });
    };
    const terminal = () => {
      refresh();
      void queryClient.invalidateQueries({ queryKey: ["characters", projectId] });
      void queryClient.invalidateQueries({ queryKey: ["project", projectId] });
      void queryClient.invalidateQueries({ queryKey: ["jobs"] });
    };
    for (const eventType of ["job.queued", "job.updated", "job.progress", "job.unit.updated"]) source.addEventListener(eventType, refresh);
    for (const eventType of ["job.completed", "job.failed", "job.cancelled", "character-detection.completed"]) source.addEventListener(eventType, terminal);
    return () => source.close();
  }, [activeDetection?.id, projectId, queryClient]);
  const filteredVoices = voices.data?.items.filter((voice) => !voiceProvider || voice.providerProfileId === voiceProvider) ?? [];
  const cloneProviders = providers.data?.items.filter((provider) => provider.capabilities?.voiceCloning) ?? [];
  const selectedDetectionProvider = aiProviders.find((provider) => provider.id === detectionProvider);
  const characterMutationPending = detection.isPending || approve.isPending || assignment.isPending || identity.isPending
    || speakerOverride.isPending || createIdentity.isPending || mergeIdentity.isPending || deleteIdentity.isPending;

  if (characters.isLoading || detectionStatus.isLoading) return <LoadingState label={t("state.loadingProject")} />;
  if (characters.isError) return <ErrorState error={characters.error} onRetry={() => void characters.refetch()} />;
  if (detectionStatus.isError) return <ErrorState error={detectionStatus.error} onRetry={() => void detectionStatus.refetch()} />;

  const characterItems = characters.data?.items ?? [];
  const noCharacters = characterItems.length === 0;
  return (
    <div>
      <PageHeading title={t("characters.title")} subtitle={t("characters.subtitle")} actions={
        <div className="cluster">
          {reviewStatus === "approved" ? <Badge tone="positive"><Check size={13} />{t("characters.approved")}</Badge> : <Badge tone="warning"><AlertTriangle size={13} />{t("characters.needsReview")}</Badge>}
          <Button variant="secondary" disabled={Boolean(activeDetection) || characterMutationPending} onClick={() => setAddingCharacter(true)}><Plus size={16} />{t("characters.addCharacter")}</Button>
          {cloneProviders.length ? <Button variant="secondary" onClick={() => setVoiceLibraryOpen(true)}><Mic2 size={16} />{t("characters.manageVoiceClones")}</Button> : null}
        </div>
      } />
      {providers.isLoading || voices.isLoading ? <LoadingState label={t("state.loadingProviders")} /> : null}
      {providers.isError ? <ErrorState error={providers.error} onRetry={() => void providers.refetch()} /> : null}
      {voices.isError ? <ErrorState error={voices.error} onRetry={() => void voices.refetch()} /> : null}
      {activeDetection ? <Card className="detection-config stack" role="status">
        <div className="space-between"><div><h2>{t("characters.detectionStatus", { status: t(`characters.detectionState_${activeDetection.status}`, { defaultValue: activeDetection.status.replace("_", " ") }) })}</h2><p>{activeDetection.currentStage ? t(`stage.${activeDetection.currentStage}`, { defaultValue: activeDetection.currentStage }) : t("characters.detectionPreparing")}</p></div><Badge tone={activeDetection.status === "paused" ? "warning" : "accent"}>{Math.round(activeDetection.progress)}%</Badge></div>
        <ProgressBar value={activeDetection.progress} label={t("characters.detectionProgress", { value: Math.round(activeDetection.progress) })} />
        <div className="cluster">
          {(["queued", "running"] as const).includes(activeDetection.status as "queued" | "running") ? <Button size="sm" variant="secondary" disabled={detectionAction.isPending} onClick={() => detectionAction.mutate("pause")}>{t("jobs.pause")}</Button> : null}
          {activeDetection.status === "paused" ? <Button size="sm" disabled={detectionAction.isPending} onClick={() => detectionAction.mutate("resume")}>{t("jobs.resume")}</Button> : null}
          {!(["cancelling", "cancelled", "complete", "failed"] as string[]).includes(activeDetection.status) ? <Button size="sm" variant="ghost" disabled={detectionAction.isPending} onClick={() => detectionAction.mutate("cancel")}>{t("jobs.cancel")}</Button> : null}
          <Link className="button button-secondary button-sm" to={`/jobs/${activeDetection.id}`}>{t("characters.openJob")}</Link>
        </div>
        <p className="muted-copy">{t("characters.detectionLocked")}</p>
        {detectionAction.isError ? <ErrorState error={detectionAction.error} onRetry={() => detectionAction.mutate(detectionAction.variables ?? "retry")} /> : null}
      </Card> : null}
      {!activeDetection && latestDetection?.status === "failed" ? <Card className="detection-config stack" role="alert">
        <div className="space-between"><div><h2>{t("characters.detectionFailedTitle")}</h2><p>{latestDetection.currentStage ? t(`stage.${latestDetection.currentStage}`, { defaultValue: latestDetection.currentStage }) : t("characters.detectionFailedDetail")}</p></div><Badge tone="danger">{t("jobs.failed")}</Badge></div>
        <div className="cluster"><Button size="sm" disabled={detectionAction.isPending} onClick={() => detectionAction.mutate("retry")}>{t("characters.retryFailedJob")}</Button><Link className="button button-secondary button-sm" to={`/jobs/${latestDetection.id}`}>{t("characters.openJob")}</Link></div>
        {detectionAction.isError ? <ErrorState error={detectionAction.error} onRetry={() => detectionAction.mutate(detectionAction.variables ?? "retry")} /> : null}
      </Card> : null}
      {aiProviders.length ? <Card className="detection-config stack">
        <div className="section-heading"><div><h2>{t("characters.detectionSettings")}</h2><p>{t("characters.detectionSettingsDetail")}</p></div></div>
        <div className="grid-2">
          <Field label={t("characters.detectionProvider")}>
            <Select value={detectionProvider} disabled={characterMutationPending} onChange={(event) => { setDetectionProvider(event.target.value); setTemperatureMode("default"); setReasoningMode("inherit"); detection.reset(); }}>
              <option value="">{t("common.select")}</option>
              {aiProviders.map((provider) => <option key={provider.id} value={provider.id}>{provider.name}</option>)}
            </Select>
          </Field>
          {selectedDetectionProvider?.capabilities?.temperature !== "unsupported" ? <Field label={t("characters.temperature")} hint={t("characters.temperatureHint")}>
            <Select value={temperatureMode} disabled={characterMutationPending} onChange={(event) => setTemperatureMode(event.target.value as DetectionTemperature["mode"])}>
              <option value="default">{t("characters.providerDefault")}</option>
              {selectedDetectionProvider?.capabilities?.temperature === "nullable" ? <option value="null">{t("characters.explicitNull")}</option> : null}
              <option value="value">{t("characters.customValue")}</option>
            </Select>
          </Field> : null}
          {temperatureMode === "value" && selectedDetectionProvider?.capabilities?.temperature !== "unsupported" ? <Field label={t("characters.temperatureValue")}>
            <Input type="number" min={0} max={2} step={0.1} disabled={characterMutationPending} value={temperatureValue} onChange={(event) => { if (Number.isFinite(event.target.valueAsNumber)) setTemperatureValue(event.target.valueAsNumber); }} />
          </Field> : null}
          {selectedDetectionProvider?.capabilities?.reasoning.length ? <Field label={t("characters.reasoning")} hint={t("characters.reasoningHint")}>
            <Select value={reasoningMode} disabled={characterMutationPending} onChange={(event) => setReasoningMode(event.target.value as DetectionReasoning["mode"])}>
              <option value="inherit">{t("characters.providerDefault")}</option>
              {selectedDetectionProvider.capabilities.reasoning.includes("disabled") ? <option value="disabled">{t("characters.reasoningDisabled")}</option> : null}
              {selectedDetectionProvider.capabilities.reasoning.includes("effort") ? <option value="effort">{t("characters.reasoningEffort")}</option> : null}
              {selectedDetectionProvider.capabilities.reasoning.includes("adaptive") ? <option value="adaptive">{t("characters.reasoningAdaptive")}</option> : null}
              {selectedDetectionProvider.capabilities.reasoning.includes("token_budget") ? <option value="token_budget">{t("characters.reasoningTokenBudget")}</option> : null}
            </Select>
          </Field> : null}
          {reasoningMode === "effort" ? <Field label={t("characters.reasoningEffort")}>
            <Select value={reasoningEffort} disabled={characterMutationPending} onChange={(event) => setReasoningEffort(event.target.value as typeof reasoningEffort)}>
              {(["minimal", "low", "medium", "high"] as const).map((effort) => <option value={effort} key={effort}>{t(`characters.effort_${effort}`)}</option>)}
            </Select>
          </Field> : null}
          {reasoningMode === "token_budget" ? <Field label={t("characters.reasoningTokens")}>
            <Input type="number" min={1024} step={256} disabled={characterMutationPending} value={reasoningTokens} onChange={(event) => { if (Number.isFinite(event.target.valueAsNumber)) setReasoningTokens(event.target.valueAsNumber); }} />
          </Field> : null}
        </div>
        <div className="space-between"><span className="muted-copy">{t("characters.detectionBilling")}</span><Button disabled={!detectionProvider || characterMutationPending || Boolean(activeDetection) || (reasoningMode === "token_budget" && reasoningTokens < 1024)} onClick={() => detection.mutate(crypto.randomUUID())}>{detection.isPending ? <LoaderCircle className="spin" size={16} /> : <Sparkles size={16} />}{noCharacters ? t("characters.detect") : t("characters.detectAgain")}</Button></div>
      </Card> : null}
      {detection.isError ? <ErrorState error={detection.error} onRetry={() => detection.mutate(retryIdempotencyKey(detection.error, detection.variables))} /> : null}
      {speakerOverride.isError ? <ErrorState error={speakerOverride.error} onRetry={speakerOverride.variables ? () => speakerOverride.mutate(speakerOverride.variables!) : undefined} /> : null}
      {noCharacters ? (
        <Card className="detection-empty">
          <div className="empty-icon"><Sparkles size={23} /></div><h2>{t("characters.notStartedTitle")}</h2><p>{t("characters.notStartedDetail")}</p>
          {!aiProviders.length && !providers.isLoading && !providers.isError ? <div className="stack"><Badge tone="warning">{t("characters.providerNeeded")}</Badge><Link className="button button-secondary button-md" to="/providers">{t("providers.add")}</Link></div> : null}
        </Card>
      ) : (
        <>
          <div className="character-grid">
            {characterItems.map((character) => (
              <Card className="character-card" key={character.id}>
                <div className="character-top"><div className="avatar">{character.canonicalName.slice(0, 1).toUpperCase()}</div><div><div className="cluster"><h2>{character.canonicalName}</h2>{character.role === "narrator" ? <Badge tone="accent">{t("characters.narratorRole")}</Badge> : null}</div><p>{t("characters.dialogue", { count: character.dialogueCount })}</p></div><div className="character-card-actions"><Badge tone={character.confidence >= .8 ? "positive" : "warning"}>{t("characters.confidence", { value: `${Math.round(character.confidence * 100)}%` })}</Badge><Button disabled={Boolean(activeDetection) || characterMutationPending} size="sm" variant="ghost" aria-label={t("characters.editIdentity", { name: character.canonicalName })} onClick={() => { setEditingIdentity(character); setIdentityName(character.canonicalName); setIdentityAliases(character.aliases.join(", ")); identity.reset(); }}><Pencil size={14} /></Button>{character.role !== "narrator" ? <><Button disabled={Boolean(activeDetection) || characterMutationPending} size="sm" variant="ghost" aria-label={t("characters.mergeCharacterLabel", { name: character.canonicalName })} onClick={() => { setMergingCharacter(character); setMergeTargetId(""); setMergeConfirmed(false); }}><UserRound size={14} /></Button><Button disabled={Boolean(activeDetection) || characterMutationPending} size="sm" variant="ghost" aria-label={t("characters.deleteCharacterLabel", { name: character.canonicalName })} onClick={() => { setDeletingCharacter(character); setDeleteCharacterConfirmed(false); }}><Trash2 size={14} /></Button></> : null}</div></div>
                {character.aliases.length ? <div className="aliases"><span>{t("characters.aliases")}</span>{character.aliases.map((alias) => <Badge key={alias}>{alias}</Badge>)}</div> : null}
                <button type="button" className="voice-assignment" disabled={Boolean(activeDetection) || characterMutationPending || providers.isLoading || providers.isError || voices.isLoading || voices.isError} onClick={() => { setAssigning(character); setVoiceProvider(character.voiceAssignment?.providerProfileId ?? ""); setVoiceId(character.voiceAssignment?.voiceId ?? ""); }}>
                  <span className="voice-icon"><Mic2 size={17} /></span><span><small>{t("characters.voice")}</small><strong>{character.voiceAssignment?.voiceName ?? t("characters.noVoice")}</strong>{character.voiceAssignment ? <em>{character.voiceAssignment.providerName}</em> : null}</span><span>{t("common.edit")}</span>
                </button>
                {character.evidence.length ? <details className="evidence"><summary><MessageSquareQuote size={15} />{t("characters.evidence")}<span>{character.evidence.length}</span></summary><div className="evidence-list">{character.evidence.map((item) => {
                  const paragraphId = paragraphIdFor(item);
                  const evidenceKey = `${paragraphId}:${item.startOffset}:${item.endOffset}`;
                  const optimisticSelection = speakerSelections[evidenceKey];
                  const selection = optimisticSelection && optimisticSelection.characterRevision > characterRevision
                    ? optimisticSelection.selection
                    : storedSpeakerSelection(item, characterItems);
                  const saving = speakerOverride.isPending && `${paragraphIdFor(speakerOverride.variables.evidence)}:${speakerOverride.variables.evidence.startOffset}:${speakerOverride.variables.evidence.endOffset}` === evidenceKey;
                  return <article className="evidence-item" key={item.id}><blockquote><p>“{item.excerpt}”</p><cite>{item.chapterTitle} · {Math.round(item.confidence * 100)}%</cite></blockquote><div className="evidence-speaker"><Field label={t("characters.speakerFor", { chapter: item.chapterTitle })}><Select value={selection} disabled={characterMutationPending || Boolean(activeDetection)} onChange={(event) => speakerOverride.mutate({ evidence: item, selection: event.target.value })}><option value={AUTO_SPEAKER}>{t("characters.detectedSpeaker", { name: character.canonicalName })}</option><option value={NARRATOR_SPEAKER}>{t("characters.narratorSpeaker")}</option>{characterItems.map((candidate) => <option key={candidate.id} value={candidate.id}>{candidate.canonicalName}</option>)}</Select></Field>{saving ? <span className="speaker-saving" role="status"><LoaderCircle className="spin" size={13} />{t("characters.correctingSpeaker")}</span> : selection !== AUTO_SPEAKER ? <Badge tone="accent">{t("characters.override")}</Badge> : null}</div></article>;
                })}</div></details> : null}
              </Card>
            ))}
          </div>
          <Card className="review-gate"><ShieldCheck size={23} /><div><strong>{reviewStatus === "approved" ? t("characters.approved") : t("characters.approve")}</strong><p>{t("characters.approveDetail")}</p></div>{reviewStatus !== "approved" ? <Button onClick={() => approve.mutate()} disabled={characterMutationPending || Boolean(activeDetection)}>{approve.isPending ? <LoaderCircle className="spin" size={16} /> : <Check size={16} />}{t("characters.approve")}</Button> : null}</Card>
          {approve.isError ? <ErrorState error={approve.error} onRetry={() => void Promise.all([characters.refetch(), queryClient.invalidateQueries({ queryKey: ["project", projectId] })])} /> : null}
          <div className="panel-footer"><Link className="button button-primary button-md" to={`/projects/${projectId}/pronunciation`}>{t("common.continue")}<Volume2 size={16} /></Link></div>
        </>
      )}

      <Dialog open={Boolean(assigning)} onOpenChange={(open) => !open && !assignment.isPending && setAssigning(undefined)} title={t("characters.assignVoice", { name: assigning?.canonicalName })} description={t("characters.subtitle")} footer={<><Button variant="secondary" disabled={assignment.isPending} onClick={() => setAssigning(undefined)}>{t("common.cancel")}</Button><Button disabled={!voiceProvider || !voiceId || characterMutationPending} onClick={() => assignment.mutate()}>{assignment.isPending ? t("state.saving") : t("common.save")}</Button></>}>
        <div className="stack">
          <Field label={t("providers.title")}><Select value={voiceProvider} disabled={characterMutationPending} onChange={(event) => { setVoiceProvider(event.target.value); setVoiceId(""); }}><option value="">{t("common.select")}</option>{providers.data?.items.filter((provider) => provider.capabilities?.tts).map((provider) => <option value={provider.id} key={provider.id}>{provider.name}</option>)}</Select></Field>
          <Field label={t("characters.voice")}><Select value={voiceId} onChange={(event) => setVoiceId(event.target.value)} disabled={!voiceProvider || characterMutationPending}><option value="">{t("common.select")}</option>{filteredVoices.map((voice) => <option value={voice.id} key={voice.id}>{voice.name}{voice.locale ? ` · ${voice.locale}` : ""}</option>)}</Select></Field>
          {providers.data?.items.find((provider) => provider.id === voiceProvider)?.capabilities?.voiceCloning ? <Button variant="secondary" onClick={() => { setAssigning(undefined); setVoiceLibraryOpen(true); }}><Plus size={16} />{t("characters.manageVoiceClones")}</Button> : null}
          {assignment.isError ? <ErrorState error={assignment.error} /> : null}
        </div>
      </Dialog>
      <Dialog open={Boolean(editingIdentity)} onOpenChange={(open) => !open && !identity.isPending && setEditingIdentity(undefined)} title={t("characters.editIdentityTitle")} description={editingIdentity?.canonicalName} footer={<><Button variant="secondary" disabled={identity.isPending} onClick={() => setEditingIdentity(undefined)}>{t("common.cancel")}</Button><Button disabled={!identityName.trim() || characterMutationPending} onClick={() => editingIdentity && identity.mutate({ characterId: editingIdentity.id, name: identityName.trim(), aliases: parseAliases(identityAliases, identityName) })}>{identity.isPending ? <LoaderCircle className="spin" size={16} /> : <Save size={16} />}{identity.isPending ? t("state.saving") : t("common.save")}</Button></>}>
        <div className="stack character-identity-form">
          <Field label={t("characters.canonicalName")}><Input autoFocus disabled={characterMutationPending} value={identityName} onChange={(event) => setIdentityName(event.target.value)} /></Field>
          <Field label={t("characters.aliases")} hint={t("characters.aliasesHint")}><Textarea aria-label={t("characters.aliases")} disabled={characterMutationPending} value={identityAliases} onChange={(event) => setIdentityAliases(event.target.value)} /></Field>
          {identity.isError ? <ErrorState error={identity.error} /> : null}
        </div>
      </Dialog>
      <Dialog open={addingCharacter} onOpenChange={(open) => !createIdentity.isPending && setAddingCharacter(open)} title={t("characters.addCharacter")} description={t("characters.addCharacterDetail")} footer={<><Button variant="secondary" disabled={createIdentity.isPending} onClick={() => setAddingCharacter(false)}>{t("common.cancel")}</Button><Button disabled={!newCharacterName.trim() || characterMutationPending || Boolean(activeDetection)} onClick={() => createIdentity.mutate(crypto.randomUUID())}>{createIdentity.isPending ? <LoaderCircle className="spin" size={16} /> : <Plus size={16} />}{t("characters.addCharacter")}</Button></>}>
        <div className="stack character-identity-form">
          <Field label={t("characters.canonicalName")}><Input autoFocus disabled={characterMutationPending} value={newCharacterName} onChange={(event) => setNewCharacterName(event.target.value)} /></Field>
          <Field label={t("characters.aliases")} hint={t("characters.aliasesHint")}><Textarea aria-label={t("characters.newCharacterAliases")} disabled={characterMutationPending} value={newCharacterAliases} onChange={(event) => setNewCharacterAliases(event.target.value)} /></Field>
          {createIdentity.isError ? <ErrorState error={createIdentity.error} onRetry={() => createIdentity.mutate(retryIdempotencyKey(createIdentity.error, createIdentity.variables))} /> : null}
        </div>
      </Dialog>
      <Dialog open={Boolean(mergingCharacter)} onOpenChange={(open) => { if (!open && !mergeIdentity.isPending) { setMergingCharacter(undefined); setMergeTargetId(""); setMergeConfirmed(false); } }} title={t("characters.mergeTitle", { name: mergingCharacter?.canonicalName ?? t("characters.characterFallback") })} description={t("characters.mergeDetail")} footer={<><Button variant="secondary" disabled={mergeIdentity.isPending} onClick={() => { setMergingCharacter(undefined); setMergeTargetId(""); setMergeConfirmed(false); }}>{t("common.cancel")}</Button><Button variant="danger" disabled={!mergeTargetId || !mergeConfirmed || characterMutationPending || Boolean(activeDetection)} onClick={() => mergeIdentity.mutate(crypto.randomUUID())}>{mergeIdentity.isPending ? <LoaderCircle className="spin" size={16} /> : <UserRound size={16} />}{t("characters.mergeAction")}</Button></>}>
        <div className="stack">
          <Field label={t("characters.mergeInto")}><Select aria-label={t("characters.mergeTarget")} disabled={characterMutationPending} value={mergeTargetId} onChange={(event) => { setMergeTargetId(event.target.value); setMergeConfirmed(false); }}><option value="">{t("common.select")}</option>{characterItems.filter((candidate) => candidate.id !== mergingCharacter?.id).map((candidate) => <option key={candidate.id} value={candidate.id}>{candidate.canonicalName}{candidate.role === "narrator" ? ` · ${t("characters.narratorRole")}` : ""}</option>)}</Select></Field>
          <SwitchField checked={mergeConfirmed} disabled={characterMutationPending || !mergeTargetId} onCheckedChange={setMergeConfirmed} label={t("characters.confirmMerge")} detail={t("characters.confirmMergeDetail")} />
          {mergeIdentity.isError ? <ErrorState error={mergeIdentity.error} onRetry={() => mergeIdentity.mutate(retryIdempotencyKey(mergeIdentity.error, mergeIdentity.variables))} /> : null}
        </div>
      </Dialog>
      <Dialog open={Boolean(deletingCharacter)} onOpenChange={(open) => { if (!open && !deleteIdentity.isPending) { setDeletingCharacter(undefined); setDeleteCharacterConfirmed(false); } }} title={t("characters.deleteTitle", { name: deletingCharacter?.canonicalName ?? t("characters.characterFallback") })} description={t("characters.deleteDetail")} footer={<><Button variant="secondary" disabled={deleteIdentity.isPending} onClick={() => { setDeletingCharacter(undefined); setDeleteCharacterConfirmed(false); }}>{t("common.cancel")}</Button><Button variant="danger" disabled={!deleteCharacterConfirmed || characterMutationPending || Boolean(activeDetection)} onClick={() => deleteIdentity.mutate(crypto.randomUUID())}>{deleteIdentity.isPending ? <LoaderCircle className="spin" size={16} /> : <Trash2 size={16} />}{t("characters.deleteAction")}</Button></>}>
        <div className="stack"><p>{t("characters.deleteResultDetail")}</p><SwitchField checked={deleteCharacterConfirmed} disabled={characterMutationPending} onCheckedChange={setDeleteCharacterConfirmed} label={t("characters.confirmDeleteCharacter")} detail={t("characters.confirmDeleteCharacterDetail")} />{deleteIdentity.isError ? <ErrorState error={deleteIdentity.error} onRetry={() => deleteIdentity.mutate(retryIdempotencyKey(deleteIdentity.error, deleteIdentity.variables))} /> : null}</div>
      </Dialog>
      <VoiceCloneManager
        projectId={projectId}
        consentCloudAudio={consentCloudAudio}
        providers={cloneProviders}
        voices={voices.data?.items ?? []}
        open={voiceLibraryOpen}
        onOpenChange={setVoiceLibraryOpen}
      />
    </div>
  );
}

function VoiceCloneManager({
  projectId,
  consentCloudAudio,
  providers,
  voices,
  open,
  onOpenChange,
}: {
  projectId: string;
  consentCloudAudio: boolean;
  providers: ProviderProfile[];
  voices: Voice[];
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [providerId, setProviderId] = useState("");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [samples, setSamples] = useState<File[]>([]);
  const [sampleInputKey, setSampleInputKey] = useState(0);
  const [editingVoice, setEditingVoice] = useState<Voice>();
  const [editName, setEditName] = useState("");
  const [deletingVoice, setDeletingVoice] = useState<Voice>();
  const [deleteConfirmed, setDeleteConfirmed] = useState(false);
  const activeProviderId = providers.some((provider) => provider.id === providerId)
    ? providerId
    : providers[0]?.id ?? "";
  const activeProvider = providers.find((provider) => provider.id === activeProviderId);
  const ownedClones = voices.filter((voice) =>
    voice.providerProfileId === activeProviderId && voice.kind === "remote_clone" && voice.owned);
  const cloudConsentRequired = activeProvider?.mode === "cloud_remote";

  const resetCreateForm = () => {
    setName("");
    setDescription("");
    setSamples([]);
    setSampleInputKey((value) => value + 1);
  };
  const create = useMutation({
    mutationFn: () => api.createVoiceClone(activeProviderId, {
      name: name.trim(),
      description: description.trim() || undefined,
      projectId,
      referenceAudio: samples,
    }),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["voices"] });
      resetCreateForm();
    },
  });
  const update = useMutation({
    mutationFn: ({ voiceId, nextName }: { voiceId: string; nextName: string }) =>
      api.updateVoiceClone(voiceId, nextName),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["voices"] });
      setEditingVoice(undefined);
      setEditName("");
    },
  });
  const remove = useMutation({
    mutationFn: (voiceId: string) => api.deleteVoiceClone(voiceId, true),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["voices"] });
      setDeletingVoice(undefined);
      setDeleteConfirmed(false);
    },
  });
  const consent = useMutation({
    mutationFn: (allowed: boolean) => api.updateProject(projectId, { consentCloudAudio: allowed }),
    onSuccess: (project) => queryClient.setQueryData(["project", projectId], project),
  });

  const close = (nextOpen: boolean) => {
    if (!nextOpen) {
      setEditingVoice(undefined);
      setDeletingVoice(undefined);
      setDeleteConfirmed(false);
      create.reset();
      update.reset();
      remove.reset();
    }
    onOpenChange(nextOpen);
  };

  return (
    <Dialog
      open={open}
      onOpenChange={close}
      title={t("characters.voiceCloneTitle")}
      description={t("characters.voiceCloneDetail")}
      size="lg"
      footer={<Button variant="secondary" onClick={() => close(false)}>{t("common.close")}</Button>}
    >
      <div className="stack voice-clone-manager">
        <Field label={t("characters.cloneProvider")}>
          <Select value={activeProviderId} onChange={(event) => { setProviderId(event.target.value); setEditingVoice(undefined); setDeletingVoice(undefined); }}>
            {providers.map((provider) => <option value={provider.id} key={provider.id}>{provider.name}</option>)}
          </Select>
        </Field>

        {cloudConsentRequired ? (
          <Card className="voice-clone-consent">
            <ShieldCheck size={20} />
            <SwitchField
              checked={consentCloudAudio}
              disabled={consent.isPending}
              onCheckedChange={(allowed) => consent.mutate(allowed)}
              label={t("characters.cloudAudioConsent")}
              detail={t("characters.cloudAudioConsentDetail")}
            />
          </Card>
        ) : null}
        {consent.isError ? <ErrorState error={consent.error} /> : null}

        <section className="voice-clone-section" aria-labelledby="owned-clones-heading">
          <div className="section-heading">
            <div><h2 id="owned-clones-heading">{t("characters.ownedClones")}</h2><p>{t("characters.ownedClonesDetail")}</p></div>
          </div>
          {ownedClones.length ? (
            <div className="voice-clone-list">
              {ownedClones.map((voice) => (
                <Card className="voice-clone-row" key={voice.id}>
                  {editingVoice?.id === voice.id ? (
                    <Field label={t("characters.renameClone", { name: voice.name })} className="voice-clone-name">
                      <Input autoFocus value={editName} onChange={(event) => setEditName(event.target.value)} />
                    </Field>
                  ) : <div className="voice-clone-name"><strong>{voice.name}</strong><Badge tone="positive">{t("characters.appOwned")}</Badge></div>}
                  <div className="cluster">
                    {editingVoice?.id === voice.id ? (
                      <>
                        <Button size="sm" variant="ghost" onClick={() => setEditingVoice(undefined)}>{t("common.cancel")}</Button>
                        <Button size="sm" disabled={!editName.trim() || update.isPending} onClick={() => update.mutate({ voiceId: voice.id, nextName: editName.trim() })}><Save size={14} />{t("common.save")}</Button>
                      </>
                    ) : (
                      <>
                        <Button size="sm" variant="ghost" aria-label={t("characters.renameClone", { name: voice.name })} onClick={() => { setEditingVoice(voice); setEditName(voice.name); update.reset(); }}><Pencil size={14} /></Button>
                        <Button size="sm" variant="ghost" aria-label={t("characters.deleteClone", { name: voice.name })} onClick={() => { setDeletingVoice(voice); setDeleteConfirmed(false); remove.reset(); }}><Trash2 size={14} /></Button>
                      </>
                    )}
                  </div>
                </Card>
              ))}
            </div>
          ) : <p className="muted-copy">{t("characters.noOwnedClones")}</p>}
          {update.isError ? <ErrorState error={update.error} /> : null}
        </section>

        {deletingVoice ? (
          <Card className="voice-clone-delete" role="alert">
            <AlertTriangle size={21} />
            <div className="stack">
              <div><strong>{t("characters.deleteCloneTitle", { name: deletingVoice.name })}</strong><p>{t("characters.deleteCloneDetail")}</p></div>
              <SwitchField checked={deleteConfirmed} onCheckedChange={setDeleteConfirmed} label={t("characters.deleteCloneConfirm")} />
              <div className="cluster">
                <Button variant="secondary" onClick={() => { setDeletingVoice(undefined); setDeleteConfirmed(false); }}>{t("common.cancel")}</Button>
                <Button variant="danger" disabled={!deleteConfirmed || remove.isPending} onClick={() => remove.mutate(deletingVoice.id)}><Trash2 size={15} />{t("characters.deleteRemoteClone")}</Button>
              </div>
              {remove.isError ? <ErrorState error={remove.error} /> : null}
            </div>
          </Card>
        ) : null}

        <section className="voice-clone-section" aria-labelledby="create-clone-heading">
          <div className="section-heading"><div><h2 id="create-clone-heading">{t("characters.createClone")}</h2><p>{t("characters.createCloneDetail")}</p></div></div>
          <div className="stack">
            <Field label={t("characters.cloneName")}><Input value={name} onChange={(event) => setName(event.target.value)} /></Field>
            <Field label={t("characters.cloneDescription")} hint={t("common.optional")}><Textarea aria-label={t("characters.cloneDescription")} value={description} onChange={(event) => setDescription(event.target.value)} /></Field>
            <Field label={t("characters.referenceAudio")} hint={t("characters.referenceAudioHint")}>
              <Input key={sampleInputKey} aria-label={t("characters.referenceAudio")} type="file" accept="audio/*" multiple onChange={(event) => setSamples(Array.from(event.target.files ?? []))} />
            </Field>
            {samples.length ? <Badge tone="info">{t("characters.referenceAudioCount", { count: samples.length })}</Badge> : null}
            {cloudConsentRequired && !consentCloudAudio ? <Badge tone="warning">{t("characters.cloudAudioBlocked")}</Badge> : null}
            {create.isError ? <ErrorState error={create.error} /> : null}
            <div className="cluster"><Button disabled={!activeProviderId || !name.trim() || samples.length === 0 || create.isPending || (cloudConsentRequired && !consentCloudAudio)} onClick={() => create.mutate()}><Upload size={16} />{create.isPending ? t("characters.creatingClone") : t("characters.createClone")}</Button></div>
          </div>
        </section>
      </div>
    </Dialog>
  );
}

function PronunciationPanel({ projectId, defaultLanguage }: { projectId: string; defaultLanguage: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const rules = useQuery({ queryKey: ["pronunciation", projectId], queryFn: () => api.pronunciationRules(projectId) });
  const characters = useQuery({ queryKey: ["characters", projectId], queryFn: () => api.characters(projectId) });
  const [open, setOpen] = useState(false);
  const [deletingRule, setDeletingRule] = useState<PronunciationRule>();
  const [previewText, setPreviewText] = useState("");
  const [previewLanguage, setPreviewLanguage] = useState(defaultLanguage);
  const [previewCharacterId, setPreviewCharacterId] = useState("");
  const [form, setForm] = useState({ source: "", replacement: "", kind: "whole_word" as PronunciationRule["kind"], scope: "project" as PronunciationRule["scope"], language: "", characterId: "", caseSensitive: false });
  const create = useMutation({
    mutationFn: () => api.createPronunciationRule({
      ...form,
      projectId: form.scope === "project" ? projectId : undefined,
      language: form.language || undefined,
      characterId: form.characterId || undefined,
      enabled: true,
      order: rules.data?.items.length ?? 0,
    }),
    onSuccess: async () => { await queryClient.invalidateQueries({ queryKey: ["pronunciation", projectId] }); setOpen(false); setForm({ source: "", replacement: "", kind: "whole_word", scope: "project", language: "", characterId: "", caseSensitive: false }); },
  });
  const remove = useMutation({
    mutationFn: (ruleId: string) => api.deletePronunciationRule(ruleId),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["pronunciation", projectId] });
      setDeletingRule(undefined);
    },
  });
  const preview = useMutation({
    mutationFn: () => api.previewPronunciationRules({
      text: previewText,
      projectId,
      language: previewLanguage.trim() || undefined,
      characterId: previewCharacterId || undefined,
    }),
  });
  if (rules.isLoading) return <LoadingState label={t("state.loadingProject")} />;
  if (rules.isError) return <ErrorState error={rules.error} onRetry={() => void rules.refetch()} />;
  const characterItems = characters.data?.items ?? [];
  const characterNames = new Map(characterItems.map((character) => [character.id, character.canonicalName]));

  return (
    <div>
      <PageHeading title={t("pronunciation.title")} subtitle={t("pronunciation.subtitle")} actions={<Button onClick={() => setOpen(true)}><Plus size={16} />{t("pronunciation.addRule")}</Button>} />
      <Card className="pronunciation-preview">
        <div className="section-heading"><div><h2>{t("pronunciation.preview")}</h2><p>{t("pronunciation.previewDetail")}</p></div></div>
        <div className="stack">
          <Field label={t("pronunciation.previewText")}><Textarea value={previewText} onChange={(event) => { setPreviewText(event.target.value); preview.reset(); }} placeholder={t("pronunciation.previewPlaceholder")} /></Field>
          <div className="grid-2">
            <Field label={t("pronunciation.language")}><Input value={previewLanguage} onChange={(event) => setPreviewLanguage(event.target.value)} /></Field>
            <Field label={t("pronunciation.character")}><Select value={previewCharacterId} onChange={(event) => setPreviewCharacterId(event.target.value)}><option value="">{t("pronunciation.allCharacters")}</option>{characterItems.map((character) => <option value={character.id} key={character.id}>{character.canonicalName}</option>)}</Select></Field>
          </div>
          <div className="cluster"><Button variant="secondary" disabled={!previewText.trim() || preview.isPending} onClick={() => preview.mutate()}><Volume2 size={16} />{preview.isPending ? t("pronunciation.previewing") : t("pronunciation.runPreview")}</Button></div>
          {preview.isError ? <ErrorState error={preview.error} /> : null}
          {preview.data ? (
            <div className="pronunciation-preview-result" aria-live="polite">
              <span>{t("pronunciation.transformedText")}</span>
              <p>{preview.data.transformedText}</p>
              <Badge tone={preview.data.appliedRuleIds.length ? "positive" : "neutral"}>{t("pronunciation.appliedRules", { count: preview.data.appliedRuleIds.length })}</Badge>
              {preview.data.conflicts.length ? <div className="pronunciation-conflicts"><strong>{t("pronunciation.previewConflicts")}</strong>{preview.data.conflicts.map((conflict) => <p key={`${conflict.ruleId}-${conflict.detail}`}><AlertTriangle size={13} />{conflict.detail}</p>)}</div> : null}
            </div>
          ) : null}
        </div>
      </Card>
      {!rules.data?.items.length ? <EmptyState title={t("pronunciation.emptyTitle")} detail={t("pronunciation.emptyDetail")} action={<Button onClick={() => setOpen(true)}><Plus size={16} />{t("pronunciation.addRule")}</Button>} /> : (
        <div className="rule-list">{rules.data.items.map((rule) => <Card className={clsx("rule-row", !rule.enabled && "disabled")} key={rule.id}><Badge tone={rule.scope === "global" ? "info" : "accent"}>{t(`pronunciation.${rule.scope}`)}</Badge><div className="rule-copy"><span>{rule.source}</span><ArrowLeft className="rule-arrow" size={15} /><strong>{rule.replacement}</strong></div><div className="cluster"><Badge>{t(`pronunciation.${rule.kind}`)}</Badge>{rule.language ? <Badge>{rule.language}</Badge> : null}{rule.characterId ? <Badge>{characterNames.get(rule.characterId) ?? t("pronunciation.character")}</Badge> : null}{rule.conflict ? <Badge tone="warning"><AlertTriangle size={12} />{t("pronunciation.conflict")}</Badge> : null}<Button size="sm" variant="ghost" onClick={() => setDeletingRule(rule)} aria-label={t("pronunciation.deleteRule", { source: rule.source })}><Trash2 size={15} /></Button></div></Card>)}</div>
      )}
      <div className="panel-footer"><Link className="button button-primary button-md" to={`/projects/${projectId}/preflight`}>{t("common.continue")}<Gauge size={16} /></Link></div>

      <Dialog open={open} onOpenChange={setOpen} title={t("pronunciation.addRule")} description={t("pronunciation.subtitle")} footer={<><Button variant="secondary" onClick={() => setOpen(false)}>{t("common.cancel")}</Button><Button disabled={!form.source.trim() || !form.replacement.trim() || create.isPending} onClick={() => create.mutate()}>{create.isPending ? t("state.saving") : t("common.add")}</Button></>}>
        <div className="stack">
          <div className="grid-2"><Field label={t("pronunciation.source")}><Input value={form.source} onChange={(event) => setForm({ ...form, source: event.target.value })} /></Field><Field label={t("pronunciation.replacement")}><Input value={form.replacement} onChange={(event) => setForm({ ...form, replacement: event.target.value })} /></Field></div>
          <div className="grid-2"><Field label={t("pronunciation.type")}><Select value={form.kind} onChange={(event) => setForm({ ...form, kind: event.target.value as PronunciationRule["kind"] })}>{(["literal", "whole_word", "regex", "alias", "phoneme"] as const).map((kind) => <option key={kind} value={kind}>{t(`pronunciation.${kind}`)}</option>)}</Select></Field><Field label={t("pronunciation.scope")}><Select value={form.scope} onChange={(event) => setForm({ ...form, scope: event.target.value as PronunciationRule["scope"] })}><option value="project">{t("pronunciation.project")}</option><option value="global">{t("pronunciation.global")}</option></Select></Field></div>
          <div className="grid-2"><Field label={t("pronunciation.language")}><Input value={form.language} onChange={(event) => setForm({ ...form, language: event.target.value })} /></Field><Field label={t("pronunciation.character")}><Select value={form.characterId} onChange={(event) => setForm({ ...form, characterId: event.target.value })}><option value="">{t("pronunciation.allCharacters")}</option>{characterItems.map((character) => <option value={character.id} key={character.id}>{character.canonicalName}</option>)}</Select></Field></div>
          <SwitchField checked={form.caseSensitive} onCheckedChange={(caseSensitive) => setForm({ ...form, caseSensitive })} label={t("pronunciation.caseSensitive")} />
          {create.isError ? <ErrorState error={create.error} /> : null}
        </div>
      </Dialog>
      <Dialog open={Boolean(deletingRule)} onOpenChange={(nextOpen) => !nextOpen && setDeletingRule(undefined)} title={t("pronunciation.deleteRuleTitle")} description={t("pronunciation.deleteRuleDetail", { source: deletingRule?.source })} size="sm" footer={<><Button variant="secondary" onClick={() => setDeletingRule(undefined)}>{t("common.cancel")}</Button><Button variant="danger" disabled={!deletingRule || remove.isPending} onClick={() => deletingRule && remove.mutate(deletingRule.id)}><Trash2 size={15} />{t("common.delete")}</Button></>}>
        <div className="stack">{deletingRule ? <div className="rule-copy"><span>{deletingRule.source}</span><ArrowLeft className="rule-arrow" size={15} /><strong>{deletingRule.replacement}</strong></div> : null}{remove.isError ? <ErrorState error={remove.error} /> : null}</div>
      </Dialog>
    </div>
  );
}

function PreflightPanel({ projectId }: { projectId: string }) {
  const { t, i18n } = useTranslation();
  const navigate = useNavigate();
  const [previewText, setPreviewText] = useState("");
  const [exportSettings, setExportSettings] = useState<ExportFormState>(() => ({ ...DEFAULT_EXPORT_SETTINGS }));
  const musicOwnershipRequired = requiresMusicOwnership(exportSettings);
  const hasBackgroundMusic = Boolean(exportSettings.backgroundMusicPath.trim());
  const estimate = useMutation({ mutationFn: () => api.estimate(projectId) });
  const dryRun = useMutation({ mutationFn: () => api.dryRun(projectId, toJobExportSettings(exportSettings)) });
  const preview = useMutation({ mutationFn: () => api.preview(projectId, previewText || undefined) });
  const start = useMutation({
    mutationFn: () => api.startJob({
      projectId,
      allowBudgetOverride: false,
      export: toJobExportSettings(exportSettings),
    }),
    onSuccess: (job) => navigate(`/jobs/${job.id}`),
  });
  useEffect(() => {
    dryRun.reset();
  }, [exportSettings]);
  const statusIcon = { pass: CheckCircle2, warning: CircleAlert, fail: XCircle, pending: Clock3 } as const;

  return (
    <div className="preflight-panel">
      <PageHeading title={t("preflight.title")} subtitle={t("preflight.subtitle")} />
      <div className="preflight-grid">
        <Card className="preflight-card">
          <div className="preflight-icon"><FlaskConical size={21} /></div><h2>{t("preflight.estimate")}</h2><p>{t("preflight.estimateDetail")}</p>
          <Button variant="secondary" disabled={estimate.isPending} onClick={() => estimate.mutate()}>{estimate.isPending ? <LoaderCircle className="spin" size={16} /> : <Gauge size={16} />}{t("preflight.runEstimate")}</Button>
        </Card>
        <Card className="preflight-card featured">
          <div className="preflight-icon"><Headphones size={21} /></div><h2>{t("preflight.preview")}</h2><p>{t("preflight.previewDetail")}</p>
          <Button variant="secondary" disabled={preview.isPending} onClick={() => preview.mutate()}>{preview.isPending ? <LoaderCircle className="spin" size={16} /> : <Play size={16} />}{t("preflight.runPreview")}</Button>
        </Card>
        <Card className="preflight-card">
          <div className="preflight-icon"><ShieldCheck size={21} /></div><h2>{t("preflight.dryRun")}</h2><p>{t("preflight.dryRunDetail")}</p>
          <Button variant="secondary" disabled={dryRun.isPending} onClick={() => dryRun.mutate()}>{dryRun.isPending ? <LoaderCircle className="spin" size={16} /> : <ShieldCheck size={16} />}{t("preflight.runDryRun")}</Button>
        </Card>
      </div>
      {estimate.isError ? <ErrorState error={estimate.error} onRetry={() => estimate.mutate()} /> : null}
      {estimate.data ? <Card className="estimate-result"><div className="grid-4"><Stat label={t("preflight.characters")} value={formatCount(estimate.data.characters, i18n.language)} /><Stat label={t("preflight.tokens")} value={formatCount(estimate.data.estimatedTokens, i18n.language)} /><Stat label={t("preflight.duration")} value={formatDuration(estimate.data.estimatedDurationSeconds, i18n.language)} /><Stat label={t("preflight.disk")} value={formatBytes(estimate.data.estimatedDiskBytes, i18n.language)} /><Stat label={t("preflight.completion")} value={estimate.data.estimatedCompletionSecondsLow == null ? "—" : `${formatDuration(estimate.data.estimatedCompletionSecondsLow, i18n.language)}–${formatDuration(estimate.data.estimatedCompletionSecondsHigh, i18n.language)}`} /><Stat label={t("preflight.cost")} value={formatMoney(estimate.data.monetaryCostMicros, estimate.data.currency, i18n.language)} /><Stat label={t("preflight.credits")} value={formatCount(estimate.data.credits, i18n.language)} /></div>{estimate.data.providerEstimates.length ? <div className="provider-estimates"><h3>{t("preflight.providerBreakdown")}</h3>{estimate.data.providerEstimates.map((provider) => <div className="provider-estimate-row" key={`${provider.providerProfileId}:${provider.model ?? "default"}`}><div><strong>{provider.providerName}</strong><span>{provider.model ?? t("preflight.providerDefaultModel")}</span></div><span>{t("preflight.providerEstimateDetail", { characters: formatCount(provider.characters, i18n.language), duration: formatDuration(provider.estimatedDurationSeconds, i18n.language), cost: formatMoney(provider.monetaryCostMicros, provider.currency, i18n.language) })}</span>{provider.priceSource ? <small>{t("preflight.pricing", { source: provider.priceSource, date: provider.priceEffectiveAt || t("common.unknown") })}</small> : null}</div>)}</div> : null}{estimate.data.priceSource ? <p className="result-note">{t("preflight.pricing", { source: estimate.data.priceSource, date: estimate.data.priceEffectiveAt || t("common.unknown") })}</p> : null}{estimate.data.unknownFields.length ? <p className="unknown-note"><CircleAlert size={15} />{t("preflight.unknown", { fields: estimate.data.unknownFields.join(", ") })}</p> : null}</Card> : null}

      <Card className="preview-workbench">
        <div><h2>{t("preflight.preview")}</h2><p>{t("preflight.previewBillable")}</p></div>
        <Field label={t("preflight.previewText")}><Textarea value={previewText} onChange={(event) => setPreviewText(event.target.value)} /></Field>
        {preview.isError ? <ErrorState error={preview.error} /> : null}
        {preview.data ? <div className="audio-result"><button type="button" className="audio-play" aria-label={t("preflight.preview")}><Play size={18} fill="currentColor" /></button><audio controls src={preview.data.audioUrl} preload="metadata" /><div><span>{formatDuration(preview.data.durationSeconds, i18n.language)}</span>{preview.data.cached ? <Badge tone="positive">{t("preflight.cached")}</Badge> : null}</div></div> : null}
      </Card>

      <Card className="export-settings-card">
        <header className="export-settings-heading">
          <span><Volume2 size={20} /></span>
          <div><h2>{t("preflight.exportSettings")}</h2><p>{t("preflight.exportDetail")}</p></div>
        </header>
        <div className="export-settings-grid">
          <Field label={t("preflight.exportFormat")} hint={t("preflight.exportFormatHint")}>
            <Select
              aria-label={t("preflight.exportFormat")}
              value={exportSettings.format}
              onChange={(event) => setExportSettings((current) => ({ ...current, format: event.target.value as ExportFormat }))}
            >
              <option value="m4b">{t("preflight.formatM4b")}</option>
              <option value="mp3">{t("preflight.formatMp3")}</option>
              <option value="m4a">{t("preflight.formatM4a")}</option>
              <option value="wav">{t("preflight.formatWav")}</option>
            </Select>
          </Field>
          <Field label={t("preflight.bitrate")} hint={t("preflight.bitrateHint")}>
            <Select
              aria-label={t("preflight.bitrate")}
              value={exportSettings.bitrateKbps}
              onChange={(event) => setExportSettings((current) => ({ ...current, bitrateKbps: Number(event.target.value) }))}
            >
              {[64, 96, 128, 192, 256, 320].map((bitrate) => <option key={bitrate} value={bitrate}>{t("preflight.bitrateValue", { value: bitrate })}</option>)}
            </Select>
          </Field>
          <Field label={t("preflight.outputDirectory")} hint={t("preflight.outputDirectoryHint")}>
            <Input
              aria-label={t("preflight.outputDirectory")}
              value={exportSettings.outputDirectory}
              placeholder={t("preflight.outputDirectoryPlaceholder")}
              onChange={(event) => setExportSettings((current) => ({ ...current, outputDirectory: event.target.value }))}
            />
          </Field>
          <Field label={t("preflight.fileName")} hint={t("preflight.fileNameHint")}>
            <Input
              aria-label={t("preflight.fileName")}
              value={exportSettings.fileName}
              placeholder={t("preflight.fileNamePlaceholder")}
              onChange={(event) => setExportSettings((current) => ({ ...current, fileName: event.target.value }))}
            />
          </Field>
          <div className="export-split-setting">
            <SwitchField
              checked={exportSettings.splitPerChapter}
              onCheckedChange={(splitPerChapter) => setExportSettings((current) => ({ ...current, splitPerChapter }))}
              label={t("preflight.splitPerChapter")}
              detail={exportSettings.splitPerChapter ? t("preflight.splitPerChapterDetail") : t("preflight.singleFileDetail")}
            />
          </div>
        </div>

        <section className="background-music-settings" aria-labelledby="background-music-heading">
          <header>
            <Volume2 size={18} />
            <div><h3 id="background-music-heading">{t("preflight.backgroundMusic")}</h3><p>{t("preflight.backgroundMusicDetail")}</p></div>
          </header>
          <div className="background-music-grid">
            <Field label={t("preflight.backgroundMusicPath")} hint={t("preflight.backgroundMusicPathHint")}>
              <Input
                aria-label={t("preflight.backgroundMusicPath")}
                value={exportSettings.backgroundMusicPath}
                placeholder={t("preflight.backgroundMusicPlaceholder")}
                onChange={(event) => setExportSettings((current) => ({
                  ...current,
                  backgroundMusicPath: event.target.value,
                  confirmBackgroundMusicOwned: false,
                }))}
              />
            </Field>
            <Field label={t("preflight.musicGain")} hint={t("preflight.musicGainHint")}>
              <Input
                aria-label={t("preflight.musicGain")}
                type="number"
                min={-60}
                max={0}
                step={1}
                disabled={!hasBackgroundMusic}
                value={exportSettings.musicGainDb}
                onChange={(event) => {
                  const musicGainDb = event.target.valueAsNumber;
                  if (Number.isFinite(musicGainDb)) setExportSettings((current) => ({ ...current, musicGainDb }));
                }}
              />
            </Field>
          </div>
          <div className="background-music-switches">
            <SwitchField
              checked={exportSettings.confirmBackgroundMusicOwned}
              onCheckedChange={(confirmBackgroundMusicOwned) => setExportSettings((current) => ({ ...current, confirmBackgroundMusicOwned }))}
              label={t("preflight.confirmMusicOwned")}
              detail={t("preflight.confirmMusicOwnedDetail")}
              disabled={!hasBackgroundMusic}
            />
            <SwitchField
              checked={exportSettings.ducking}
              onCheckedChange={(ducking) => setExportSettings((current) => ({ ...current, ducking }))}
              label={t("preflight.ducking")}
              detail={t("preflight.duckingDetail")}
              disabled={!hasBackgroundMusic}
            />
          </div>
          {musicOwnershipRequired ? <p className="music-ownership-warning" role="alert"><AlertTriangle size={16} />{t("preflight.musicOwnershipRequired")}</p> : null}
        </section>
      </Card>

      {dryRun.isError ? <ErrorState error={dryRun.error} onRetry={() => dryRun.mutate()} /> : null}
      {dryRun.data ? <Card className="dry-run-result"><div className="dry-run-summary">{dryRun.data.ready ? <CheckCircle2 size={24} /> : <XCircle size={24} />}<div><strong>{dryRun.data.ready ? t("preflight.ready") : t("preflight.notReady")}</strong><span>{new Date(dryRun.data.checkedAt).toLocaleString(i18n.language)}</span></div></div><div className="check-list">{dryRun.data.checks.map((check) => { const Icon = statusIcon[check.status]; return <div className={`check-row check-${check.status}`} key={check.id}><Icon size={18} /><div><strong>{check.label}</strong><p>{check.detail}</p></div><Badge tone={check.status === "pass" ? "positive" : check.status === "warning" ? "warning" : check.status === "fail" ? "danger" : "neutral"}>{t(`preflight.${check.status}`)}</Badge></div>; })}</div></Card> : null}
      {start.isError ? <ErrorState error={start.error} onRetry={() => start.mutate()} /> : null}
      <Card className="start-conversion"><div><strong>{t("shell.createAudiobook")}</strong><p>{musicOwnershipRequired ? t("preflight.musicOwnershipRequired") : dryRun.data?.ready ? t("preflight.ready") : t("preflight.startBlocked")}</p></div><Button size="lg" disabled={!dryRun.data?.ready || musicOwnershipRequired || start.isPending} onClick={() => start.mutate()}>{start.isPending ? <LoaderCircle className="spin" size={17} /> : <Sparkles size={17} />}{t("preflight.start")}</Button></Card>
    </div>
  );
}
