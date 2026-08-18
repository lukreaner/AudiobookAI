import { useMutation, useQuery } from "@tanstack/react-query";
import { AlertTriangle, Check, Headphones, LoaderCircle, Plus, Sparkles, Trash2, Volume2 } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "../api/client";
import type { DeliveryCue, ModelPerformanceCapabilities, PerformanceRange, PerformanceSettings, ProviderProfile, VoiceAuditionCandidateInput, VoiceAuditionInput } from "../api/types";
import { ErrorState, LoadingState } from "../components/StateViews";
import { Badge, Button, Card, Field, Input, Select, SwitchField, Textarea } from "../components/ui";

type CandidateDraft = VoiceAuditionCandidateInput;

interface CandidateSnapshot {
  label: string;
  providerName: string;
  voiceName: string;
}

interface AuditionSubmission {
  input: VoiceAuditionInput;
  idempotencyKey: string;
  candidates: Record<string, CandidateSnapshot>;
}

function newCandidate(): CandidateDraft {
  return {
    candidateId: crypto.randomUUID(),
    providerProfileId: "",
    voiceId: "",
    performance: {},
  };
}

function optionalNumber(value: string): number | undefined {
  if (!value.trim()) return undefined;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : undefined;
}

function descriptorFor(candidate: CandidateDraft, providers: ProviderProfile[]): ModelPerformanceCapabilities | undefined {
  const provider = providers.find((item) => item.id === candidate.providerProfileId);
  const model = candidate.model?.trim() || provider?.model?.trim();
  if (!model) return undefined;
  return provider?.capabilities?.modelPerformance.find((descriptor) => descriptor.model === model);
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

function rangeHint(range?: PerformanceRange | null): string {
  return range ? `${range.minimum}–${range.maximum}` : "—";
}

function auditionProviderReady(provider: ProviderProfile): boolean {
  const updatedAt = provider.capabilityUpdatedAt ? Date.parse(provider.capabilityUpdatedAt) : Number.NaN;
  return provider.role === "tts" && provider.status === "online" && Number.isFinite(updatedAt) && updatedAt + 24 * 60 * 60 * 1000 > Date.now() && provider.capabilities?.tts === true;
}

export function VoiceAuditionPanel({ projectId }: { projectId: string }) {
  const { t } = useTranslation();
  const providers = useQuery({ queryKey: ["providers"], queryFn: api.providers });
  const voices = useQuery({ queryKey: ["voices"], queryFn: () => api.voices() });
  const characters = useQuery({ queryKey: ["characters", projectId], queryFn: () => api.characters(projectId) });
  const [text, setText] = useState("");
  const [characterId, setCharacterId] = useState("");
  const [confirmed, setConfirmed] = useState(false);
  const [candidates, setCandidates] = useState<CandidateDraft[]>([newCandidate()]);
  const [submissionKey, setSubmissionKey] = useState(() => crypto.randomUUID());
  const [submittedCandidates, setSubmittedCandidates] = useState<Record<string, CandidateSnapshot>>();

  const ttsProviders = providers.data?.items.filter(auditionProviderReady) ?? [];
  const complete = candidates.length >= 1 && candidates.length <= 6 && candidates.every((candidate) =>
    ttsProviders.some((provider) => provider.id === candidate.providerProfileId)
      && voices.data?.items.some((voice) => voice.id === candidate.voiceId && voice.providerProfileId === candidate.providerProfileId),
  );
  const audition = useMutation({
    mutationFn: ({ input, idempotencyKey }: AuditionSubmission) => {
      if (!input.confirmBillable) throw new Error(t("auditions.confirmRequired"));
      return api.voiceAuditions(projectId, input, idempotencyKey);
    },
    onMutate: ({ candidates: submitted }) => setSubmittedCandidates(submitted),
    onSuccess: () => setSubmissionKey(crypto.randomUUID()),
    onSettled: () => setConfirmed(false),
  });

  const invalidateSubmission = () => {
    setConfirmed(false);
    setSubmittedCandidates(undefined);
    setSubmissionKey(crypto.randomUUID());
    audition.reset();
  };

  const updateCandidate = (candidateId: string, patch: Partial<CandidateDraft>) => {
    setCandidates((current) => current.map((candidate) => candidate.candidateId === candidateId ? { ...candidate, ...patch } : candidate));
    invalidateSubmission();
  };
  const updatePerformance = (candidateId: string, patch: Partial<PerformanceSettings>) => {
    setCandidates((current) => current.map((candidate) => candidate.candidateId === candidateId
      ? { ...candidate, performance: { ...candidate.performance, ...patch } }
      : candidate));
    invalidateSubmission();
  };
  const runAudition = () => {
    const candidateSnapshot = Object.fromEntries(candidates.map((candidate, index) => {
      const provider = ttsProviders.find((item) => item.id === candidate.providerProfileId);
      const voice = voices.data?.items.find((item) => item.id === candidate.voiceId && item.providerProfileId === candidate.providerProfileId);
      return [candidate.candidateId, {
        label: t("auditions.candidate", { count: index + 1 }),
        providerName: provider?.name ?? candidate.providerProfileId,
        voiceName: voice?.name ?? candidate.voiceId,
      }];
    }));
    audition.mutate({
      idempotencyKey: submissionKey,
      candidates: candidateSnapshot,
      input: {
        text: text.trim() || undefined,
        characterId: characterId || undefined,
        confirmBillable: confirmed,
        candidates: candidates.map((candidate) => ({
          ...candidate,
          performance: supportedPerformance(candidate.performance, descriptorFor(candidate, ttsProviders)),
        })),
      },
    });
  };

  if (providers.isLoading || voices.isLoading) return <LoadingState label={t("auditions.loading")} />;
  if (providers.isError) return <ErrorState error={providers.error} onRetry={() => void providers.refetch()} />;
  if (voices.isError) return <ErrorState error={voices.error} onRetry={() => void voices.refetch()} />;

  return (
    <div className="audition-workbench stack">
      <div className="section-heading audition-heading">
        <div><p className="eyebrow">{t("auditions.eyebrow")}</p><h2>{t("auditions.title")}</h2><p>{t("auditions.subtitle")}</p></div>
        <Badge tone="warning"><AlertTriangle size={13} />{t("auditions.billableBadge")}</Badge>
      </div>

      <Card className="audition-setup stack">
        <div className="audition-context-grid">
          <Field label={t("auditions.character")} hint={t("auditions.characterHint")}>
            <Select value={characterId} disabled={audition.isPending || characters.isLoading || characters.isError} onChange={(event) => { setCharacterId(event.target.value); invalidateSubmission(); }}>
              <option value="">{characters.isLoading ? t("auditions.loadingCharacters") : t("auditions.projectDefault")}</option>
              {characters.data?.items.map((character) => <option value={character.id} key={character.id}>{character.canonicalName}</option>)}
            </Select>
          </Field>
          <Field label={t("auditions.text")} hint={t("auditions.textHint")}>
            <Textarea value={text} disabled={audition.isPending} onChange={(event) => { setText(event.target.value); invalidateSubmission(); }} placeholder={t("auditions.textPlaceholder")} />
          </Field>
        </div>
        {characters.isError ? <ErrorState error={characters.error} onRetry={() => void characters.refetch()} /> : null}

        <div className="space-between audition-candidate-heading">
          <div><h3>{t("auditions.candidates")}</h3><p>{t("auditions.candidateLimit")}</p></div>
          <Button variant="secondary" size="sm" disabled={candidates.length >= 6 || audition.isPending} onClick={() => { setCandidates((current) => [...current, newCandidate()]); invalidateSubmission(); }}><Plus size={15} />{t("auditions.addCandidate")}</Button>
        </div>

        <div className="audition-candidates">
          {candidates.map((candidate, index) => {
            const availableVoices = voices.data?.items.filter((voice) => voice.providerProfileId === candidate.providerProfileId) ?? [];
            const descriptor = descriptorFor(candidate, ttsProviders);
            const capability = descriptor?.performance;
            return (
              <Card className="audition-candidate" key={candidate.candidateId}>
                <div className="space-between">
                  <div className="cluster"><span className="audition-index">{index + 1}</span><strong>{t("auditions.candidate", { count: index + 1 })}</strong></div>
                  <Button variant="ghost" size="sm" aria-label={t("auditions.removeCandidate", { count: index + 1 })} disabled={candidates.length === 1 || audition.isPending} onClick={() => { setCandidates((current) => current.filter((item) => item.candidateId !== candidate.candidateId)); invalidateSubmission(); }}><Trash2 size={15} /></Button>
                </div>
                <div className="audition-candidate-grid">
                  <Field label={t("auditions.provider")}>
                    <Select value={candidate.providerProfileId} disabled={audition.isPending} onChange={(event) => updateCandidate(candidate.candidateId, { providerProfileId: event.target.value, voiceId: "", model: undefined, performance: {} })}>
                      <option value="">{t("common.select")}</option>
                      {ttsProviders.map((provider) => <option value={provider.id} key={provider.id}>{provider.name}</option>)}
                    </Select>
                  </Field>
                  <Field label={t("auditions.voice")}>
                    <Select value={candidate.voiceId} disabled={!candidate.providerProfileId || audition.isPending} onChange={(event) => updateCandidate(candidate.candidateId, { voiceId: event.target.value })}>
                      <option value="">{t("common.select")}</option>
                      {availableVoices.map((voice) => <option value={voice.id} key={voice.id}>{voice.name}{voice.locale ? ` · ${voice.locale}` : ""}</option>)}
                    </Select>
                  </Field>
                  <Field label={t("auditions.model")} hint={t("auditions.modelHint")}>
                    <Input value={candidate.model ?? ""} disabled={audition.isPending} onChange={(event) => {
                      const model = event.target.value || undefined;
                      const next = { ...candidate, model };
                      updateCandidate(candidate.candidateId, { model, performance: supportedPerformance(candidate.performance, descriptorFor(next, ttsProviders)) });
                    }} />
                  </Field>
                </div>
                <details className="audition-direction">
                  <summary>{t("auditions.direction")}</summary>
                  {!descriptor ? <p className="audition-capability-note"><AlertTriangle size={14} />{t("auditions.directionUnsupported")}</p> : <p className="audition-capability-note supported"><Check size={14} />{t("auditions.directionVerified", { model: descriptor.model })}</p>}
                  <div className="performance-grid">
                    <Field label={t("proofing.speed")} hint={rangeHint(capability?.speed)}><Input aria-label={t("proofing.speed")} disabled={!capability?.speed || audition.isPending} type="number" min={capability?.speed?.minimum} max={capability?.speed?.maximum} step="0.05" value={candidate.performance.speed ?? ""} onChange={(event) => updatePerformance(candidate.candidateId, { speed: optionalNumber(event.target.value) })} /></Field>
                    <Field label={t("proofing.pitch")} hint={rangeHint(capability?.pitch)}><Input aria-label={t("proofing.pitch")} disabled={!capability?.pitch || audition.isPending} type="number" min={capability?.pitch?.minimum} max={capability?.pitch?.maximum} step="0.05" value={candidate.performance.pitch ?? ""} onChange={(event) => updatePerformance(candidate.candidateId, { pitch: optionalNumber(event.target.value) })} /></Field>
                    <Field label={t("proofing.stability")} hint={rangeHint(capability?.stability)}><Input aria-label={t("proofing.stability")} disabled={!capability?.stability || audition.isPending} type="number" min={capability?.stability?.minimum} max={capability?.stability?.maximum} step="0.05" value={candidate.performance.stability ?? ""} onChange={(event) => updatePerformance(candidate.candidateId, { stability: optionalNumber(event.target.value) })} /></Field>
                    <Field label={t("proofing.similarity")} hint={rangeHint(capability?.similarity)}><Input aria-label={t("proofing.similarity")} disabled={!capability?.similarity || audition.isPending} type="number" min={capability?.similarity?.minimum} max={capability?.similarity?.maximum} step="0.05" value={candidate.performance.similarity ?? ""} onChange={(event) => updatePerformance(candidate.candidateId, { similarity: optionalNumber(event.target.value) })} /></Field>
                    <Field label={t("proofing.style")} hint={rangeHint(capability?.style)}><Input aria-label={t("proofing.style")} disabled={!capability?.style || audition.isPending} type="number" min={capability?.style?.minimum} max={capability?.style?.maximum} step="0.05" value={candidate.performance.style ?? ""} onChange={(event) => updatePerformance(candidate.candidateId, { style: optionalNumber(event.target.value) })} /></Field>
                    <Field label={t("proofing.deliveryCue")}><Select aria-label={t("proofing.deliveryCue")} disabled={!capability?.delivery_cues.length || audition.isPending} value={candidate.performance.delivery_cue ?? ""} onChange={(event) => updatePerformance(candidate.candidateId, { delivery_cue: (event.target.value || undefined) as DeliveryCue | undefined })}><option value="">{t("proofing.inherit")}</option>{(capability?.delivery_cues ?? []).map((cue) => <option key={cue} value={cue}>{t(`proofing.cue.${cue}`)}</option>)}</Select></Field>
                    <Field label={t("proofing.speakerBoost")}><Select aria-label={t("proofing.speakerBoost")} disabled={!capability?.speaker_boost || audition.isPending} value={candidate.performance.speaker_boost == null ? "" : String(candidate.performance.speaker_boost)} onChange={(event) => updatePerformance(candidate.candidateId, { speaker_boost: event.target.value === "" ? undefined : event.target.value === "true" })}><option value="">{t("proofing.inherit")}</option><option value="true">{t("common.on")}</option><option value="false">{t("common.off")}</option></Select></Field>
                  </div>
                </details>
              </Card>
            );
          })}
        </div>

        {!ttsProviders.length ? <Card className="inline-notice"><AlertTriangle size={18} /><p>{t("auditions.noProviders")}</p></Card> : null}
        <div className="audition-confirmation">
          <SwitchField checked={confirmed} disabled={audition.isPending} onCheckedChange={setConfirmed} label={t("auditions.confirmBillable")} detail={t("auditions.confirmBillableDetail")} />
          <Button size="lg" disabled={!confirmed || !complete || audition.isPending} onClick={runAudition}>{audition.isPending ? <LoaderCircle className="spin" size={17} /> : <Sparkles size={17} />}{audition.isPending ? t("auditions.running") : t("auditions.run")}</Button>
        </div>
        {audition.isError ? <ErrorState error={audition.error} /> : null}
      </Card>

      {audition.data ? (
        <section className="audition-results" aria-labelledby="audition-results-heading">
          <div className="section-heading"><div><h2 id="audition-results-heading">{t("auditions.results")}</h2><p>{t("auditions.resultsDetail")}</p></div></div>
          <div className="audition-result-grid">
            {audition.data.results.map((result) => {
              const candidate = submittedCandidates?.[result.candidateId];
              return <Card className="audition-result" key={result.candidateId}>
                <div className="cluster"><span className="audio-result-icon"><Headphones size={19} /></span><div><strong>{candidate?.label ?? result.candidateId}</strong><p>{candidate?.voiceName ?? result.voiceId} · {candidate?.providerName ?? result.providerProfileId}</p></div></div>
                {result.preview ? <><audio aria-label={t("auditions.audioLabel", { candidate: candidate?.label ?? result.candidateId })} controls preload="metadata" src={result.preview.audioUrl}>{t("auditions.audioUnsupported")}</audio><div className="cluster"><Badge tone="positive"><Volume2 size={12} />{t("auditions.ready")}</Badge>{result.preview.cached ? <Badge>{t("preflight.cached")}</Badge> : null}</div></> : <Card className="audition-result-error"><AlertTriangle size={17} /><p>{result.error ?? t("auditions.failed")}</p></Card>}
              </Card>;
            })}
          </div>
        </section>
      ) : null}
    </div>
  );
}
