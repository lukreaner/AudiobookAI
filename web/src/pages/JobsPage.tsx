import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, ArrowLeft, Box, Check, ChevronRight, CircleStop, Clock3, Headphones, LoaderCircle, Pause, Play, RefreshCw, RotateCcw, Volume2 } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link, useParams } from "react-router-dom";
import { api, jobEventsUrl, playbackSocketUrl } from "../api/client";
import type { Job } from "../api/types";
import { EmptyState, ErrorState, LoadingState } from "../components/StateViews";
import { Badge, Button, Card, PageHeading, ProgressBar } from "../components/ui";
import { formatDuration, formatRelative } from "../lib/format";

function jobTone(status: Job["status"]): "neutral" | "accent" | "positive" | "warning" | "danger" {
  if (status === "complete") return "positive";
  if (status === "failed" || status === "cancelled") return "danger";
  if (status === "paused" || status === "pausing" || status === "cancelling") return "warning";
  if (status === "running") return "accent";
  return "neutral";
}

export function JobsPage() {
  const { id } = useParams();
  return id ? <JobDetail jobId={id} /> : <JobList />;
}

function JobList() {
  const { t, i18n } = useTranslation();
  const jobs = useQuery({ queryKey: ["jobs"], queryFn: api.jobs, refetchInterval: 10_000 });
  if (jobs.isLoading) return <LoadingState label={t("state.loadingJobs")} />;
  if (jobs.isError) return <ErrorState error={jobs.error} onRetry={() => void jobs.refetch()} />;
  const active = jobs.data?.items.filter((job) => ["queued", "running", "pausing", "paused", "cancelling"].includes(job.status)) ?? [];
  const history = jobs.data?.items.filter((job) => !["queued", "running", "pausing", "paused", "cancelling"].includes(job.status)) ?? [];
  return (
    <div className="page jobs-page">
      <PageHeading eyebrow={t("jobs.eyebrow")} title={t("jobs.title")} subtitle={t("jobs.subtitle")} />
      {!jobs.data?.items.length ? <EmptyState title={t("jobs.emptyTitle")} detail={t("jobs.emptyDetail")} /> : (
        <>
          {active.length ? <JobSection title={t("jobs.active")} jobs={active} locale={i18n.language} /> : null}
          {history.length ? <JobSection title={t("jobs.history")} jobs={history} locale={i18n.language} /> : null}
        </>
      )}
    </div>
  );
}

function JobSection({ title, jobs, locale }: { title: string; jobs: Job[]; locale: string }) {
  const { t } = useTranslation();
  return (
    <section className="job-section">
      <div className="section-heading"><h2>{title}</h2><span>{jobs.length}</span></div>
      <div className="job-list">
        {jobs.map((job) => (
          <Link className="job-card card" key={job.id} to={`/jobs/${job.id}`}>
            <div className="job-icon"><Box size={20} /></div>
            <div className="job-copy"><div className="cluster"><h3>{job.projectTitle}</h3><Badge>{job.kind.replaceAll("_", " ")}</Badge><Badge tone={jobTone(job.status)}>{t(`jobs.${job.status}`)}</Badge></div><p>{job.currentStage ? t("jobs.now", { stage: t(`stage.${job.currentStage}`, { defaultValue: job.currentStage }) }) : t("jobs.updated", { value: formatRelative(job.updatedAt, locale) })}</p><ProgressBar value={job.progress} label={t("jobs.progress", { value: Math.round(job.progress) })} tone={job.status === "failed" ? "warning" : "accent"} /></div>
            <div className="job-time">{job.estimatedRemainingSeconds ? <><Clock3 size={14} />{t("jobs.remaining", { value: formatDuration(job.estimatedRemainingSeconds, locale) })}</> : <>{Math.round(job.progress)}%</>}</div>
            <ChevronRight size={18} className="job-chevron" />
          </Link>
        ))}
      </div>
    </section>
  );
}

function JobDetail({ jobId }: { jobId: string }) {
  const { t, i18n } = useTranslation();
  const queryClient = useQueryClient();
  const job = useQuery({ queryKey: ["job", jobId], queryFn: () => api.job(jobId), refetchInterval: 15_000 });
  const action = useMutation({
    mutationFn: (value: "pause" | "resume" | "cancel" | "retry") => api.jobAction(jobId, value),
    onSuccess: (value) => queryClient.setQueryData(["job", jobId], value),
  });
  useEffect(() => {
    const source = new EventSource(jobEventsUrl(jobId), { withCredentials: true });
    const refresh = () => void queryClient.invalidateQueries({ queryKey: ["job", jobId] });
    for (const eventType of [
      "job.queued",
      "job.updated",
      "job.progress",
      "job.unit.updated",
      "job.completed",
      "job.failed",
    ]) {
      source.addEventListener(eventType, refresh);
    }
    return () => source.close();
  }, [jobId, queryClient]);

  if (job.isLoading) return <LoadingState label={t("state.loadingJobs")} />;
  if (job.isError) return <ErrorState error={job.error} onRetry={() => void job.refetch()} />;
  if (!job.data) return null;
  const value = job.data;
  return (
    <div className="page job-detail-page">
      <Link className="back-link" to="/jobs"><ArrowLeft size={16} />{t("jobs.title")}</Link>
      <PageHeading title={value.projectTitle} subtitle={value.currentStage ? t("jobs.now", { stage: t(`stage.${value.currentStage}`, { defaultValue: value.currentStage }) }) : t(`jobs.${value.status}`)} actions={<div className="cluster">{["queued", "running"].includes(value.status) ? <Button variant="secondary" onClick={() => action.mutate("pause")} disabled={action.isPending}><Pause size={16} />{t("jobs.pause")}</Button> : null}{value.status === "paused" ? <Button onClick={() => action.mutate("resume")} disabled={action.isPending}><Play size={16} />{t("jobs.resume")}</Button> : null}{value.status === "failed" ? <Button onClick={() => action.mutate("retry")} disabled={action.isPending}><RotateCcw size={16} />{t("jobs.retry")}</Button> : null}{!["complete", "cancelled"].includes(value.status) ? <Button variant="ghost" onClick={() => action.mutate("cancel")} disabled={action.isPending}><CircleStop size={16} />{t("jobs.cancel")}</Button> : null}</div>} />
      {action.isError ? <ErrorState error={action.error} /> : null}
      {value.uncertainCharge ? <Card className="uncertain-charge" role="alert"><AlertTriangle size={20} /><p>{t("jobs.uncertainCharge")}</p></Card> : null}
      <Card className="job-overview">
        <div className="job-overview-top"><div><Badge tone={jobTone(value.status)}>{t(`jobs.${value.status}`)}</Badge><strong>{t("jobs.progress", { value: Math.round(value.progress) })}</strong></div><span>{value.estimatedRemainingSeconds ? t("jobs.remaining", { value: formatDuration(value.estimatedRemainingSeconds, i18n.language) }) : t("jobs.updated", { value: formatRelative(value.updatedAt, i18n.language) })}</span></div>
        <ProgressBar value={value.progress} label={t("jobs.progress", { value: Math.round(value.progress) })} />
      </Card>
      {value.progressivePlaybackUrl || value.status === "running" ? <ProgressivePlayer jobId={jobId} /> : null}
      <div className="section-heading"><h2>{t("jobs.units")}</h2><span>{value.units.length}</span></div>
      <div className="unit-list">
        {value.units.map((unit) => (
          <Card className="unit-row" key={unit.id}>
            <span className={`unit-status unit-${unit.status}`}>{unit.status === "complete" ? <Check size={15} /> : unit.status === "running" ? <LoaderCircle className="spin" size={15} /> : unit.status === "failed" ? <AlertTriangle size={15} /> : <Clock3 size={15} />}</span>
            <div><strong>{unit.title}</strong><p>{t(`stage.${unit.stage}`)} · {t("jobs.attempt", { count: unit.attempt })}</p>{unit.lastError ? <small>{unit.lastError}</small> : null}</div>
            <div className="unit-progress"><span>{Math.round(unit.progress)}%</span><ProgressBar value={unit.progress} label={t("jobs.progress", { value: Math.round(unit.progress) })} /></div>
            <Badge tone={unit.status === "complete" ? "positive" : unit.status === "failed" ? "danger" : unit.status === "running" ? "accent" : unit.status === "paused" ? "warning" : "neutral"}>{t(`jobs.${unit.status}`)}</Badge>
          </Card>
        ))}
      </div>
    </div>
  );
}

const MAX_PLAYBACK_RECONNECTS = 6;

export function ProgressivePlayer({ jobId }: { jobId: string }) {
  const { t } = useTranslation();
  const [state, setState] = useState<"idle" | "connecting" | "playing" | "reconnecting" | "error">("idle");
  const contextRef = useRef<AudioContext | undefined>(undefined);
  const socketRef = useRef<WebSocket | undefined>(undefined);
  const nodeRef = useRef<AudioWorkletNode | undefined>(undefined);
  const retryTimerRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const reconnectCountRef = useRef(0);
  const stoppedRef = useRef(true);

  const stop = () => {
    stoppedRef.current = true;
    if (retryTimerRef.current !== undefined) clearTimeout(retryTimerRef.current);
    retryTimerRef.current = undefined;
    socketRef.current?.close();
    nodeRef.current?.disconnect();
    void contextRef.current?.close();
    socketRef.current = undefined;
    contextRef.current = undefined;
    nodeRef.current = undefined;
    setState("idle");
  };
  useEffect(() => stop, []);

  const play = async () => {
    setState("connecting");
    try {
      const context = new AudioContext({ sampleRate: 48_000 });
      await context.audioWorklet.addModule("/pcm-player.worklet.js");
      const node = new AudioWorkletNode(context, "audiobookai-pcm-player", { outputChannelCount: [1] });
      node.connect(context.destination);
      contextRef.current = context;
      nodeRef.current = node;
      reconnectCountRef.current = 0;
      stoppedRef.current = false;

      const connect = () => {
        if (stoppedRef.current) return;
        const socket = new WebSocket(playbackSocketUrl(jobId));
        socket.binaryType = "arraybuffer";
        socketRef.current = socket;
        socket.onopen = () => {
          if (socketRef.current === socket && !stoppedRef.current) setState("playing");
        };
        socket.onmessage = (event) => {
          if (socketRef.current !== socket || stoppedRef.current) return;
          if (event.data instanceof ArrayBuffer) {
            node.port.postMessage(new Float32Array(event.data), [event.data]);
          } else if (typeof event.data === "string") {
            try {
              if ((JSON.parse(event.data) as { type?: string }).type === "reset") {
                node.port.postMessage({ type: "clear" });
              }
            } catch {
              // Ignore forward-compatible control messages we do not understand yet.
            }
          }
        };
        socket.onerror = () => socket.close();
        socket.onclose = () => {
          if (socketRef.current !== socket || stoppedRef.current) return;
          socketRef.current = undefined;
          if (reconnectCountRef.current >= MAX_PLAYBACK_RECONNECTS) {
            stoppedRef.current = true;
            node.disconnect();
            void context.close();
            nodeRef.current = undefined;
            contextRef.current = undefined;
            setState("error");
            return;
          }
          const delay = Math.min(8_000, 500 * (2 ** reconnectCountRef.current));
          reconnectCountRef.current += 1;
          setState("reconnecting");
          retryTimerRef.current = setTimeout(connect, delay);
        };
      };

      await context.resume();
      connect();
    } catch {
      stoppedRef.current = true;
      nodeRef.current?.disconnect();
      void contextRef.current?.close();
      socketRef.current = undefined;
      contextRef.current = undefined;
      nodeRef.current = undefined;
      setState("error");
    }
  };
  const active = state === "playing" || state === "connecting" || state === "reconnecting";
  return (
    <Card className="progressive-player">
      <button type="button" className="player-control" onClick={() => active ? stop() : void play()} aria-label={active ? t("common.close") : t("jobs.playback")}>{state === "connecting" ? <LoaderCircle className="spin" size={18} /> : active ? <Pause size={18} fill="currentColor" /> : <Play size={18} fill="currentColor" />}</button>
      <div className="player-copy"><strong>{t("jobs.playback")}</strong><span>{state === "reconnecting" ? t("jobs.reconnecting") : state === "playing" ? t("jobs.now", { stage: t("stage.synthesize") }) : t("jobs.playbackWaiting")}</span></div>
      <div className="waveform" aria-hidden="true">{Array.from({ length: 32 }).map((_, index) => <i key={index} style={{ height: `${6 + ((index * 11) % 22)}px` }} />)}</div>
      <Volume2 size={17} />
    </Card>
  );
}
