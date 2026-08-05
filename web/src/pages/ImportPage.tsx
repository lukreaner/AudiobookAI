import { useMutation, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, ArrowLeft, BookOpen, Check, FileUp, LoaderCircle, ShieldCheck } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link, useLocation, useNavigate } from "react-router-dom";
import { api } from "../api/client";
import type { ImportDraft } from "../api/types";
import { ErrorState } from "../components/StateViews";
import { Button, Card, Field, Input, PageHeading, SwitchField } from "../components/ui";

export function ImportPage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const location = useLocation();
  const queryClient = useQueryClient();
  const fileInput = useRef<HTMLInputElement>(null);
  const [dragging, setDragging] = useState(false);
  const [draft, setDraft] = useState<ImportDraft>();
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [metadata, setMetadata] = useState({ title: "", author: "", language: "", series: "", seriesPosition: "", outputName: "" });
  const [cloudText, setCloudText] = useState(false);
  const [cloudAudio, setCloudAudio] = useState(false);

  const inspect = useMutation({
    mutationFn: (source: File | string) => typeof source === "string" ? api.createImportDraftFromPath(source) : api.createImportDraft(source),
    onSuccess: (value) => {
      setDraft(value);
      setSelected(new Set(value.chapters.filter((chapter) => chapter.selected).map((chapter) => chapter.id)));
      setMetadata({ title: value.title, author: value.author ?? "", language: value.language ?? "", series: "", seriesPosition: "", outputName: value.title });
    },
  });
  useEffect(() => {
    const sourcePath = (location.state as { sourcePath?: string } | null)?.sourcePath;
    if (sourcePath && !inspect.isPending && !inspect.data) inspect.mutate(sourcePath);
  }, [location.state]);
  const commit = useMutation({
    mutationFn: async () => {
      if (!draft) throw new Error("Missing import draft");
      const project = await api.commitImport(draft.draftId, [...selected]);
      return api.updateProject(project.id, {
        title: metadata.title,
        author: metadata.author || undefined,
        language: metadata.language || undefined,
        series: metadata.series || undefined,
        seriesPosition: metadata.seriesPosition ? Number(metadata.seriesPosition) : undefined,
        outputName: metadata.outputName || undefined,
        consentCloudText: cloudText,
        consentCloudAudio: cloudAudio,
      });
    },
    onSuccess: async (project) => {
      await queryClient.invalidateQueries({ queryKey: ["projects"] });
      navigate(`/projects/${project.id}/chapters`);
    },
  });

  const chooseFile = (file?: File) => {
    if (!file) return;
    inspect.reset();
    inspect.mutate(file);
  };

  return (
    <div className="page import-page">
      <Link to="/library" className="back-link"><ArrowLeft size={16} />{t("common.back")}</Link>
      <PageHeading title={t("import.title")} subtitle={t("import.subtitle")} />

      {!draft && !inspect.isPending ? (
        <>
          {inspect.isError ? <ErrorState error={inspect.error} onRetry={() => fileInput.current?.click()} /> : null}
          <button
            type="button"
            className={`drop-zone ${dragging ? "dragging" : ""}`}
            onClick={() => fileInput.current?.click()}
            onDragEnter={(event) => { event.preventDefault(); setDragging(true); }}
            onDragOver={(event) => event.preventDefault()}
            onDragLeave={() => setDragging(false)}
            onDrop={(event) => { event.preventDefault(); setDragging(false); chooseFile(event.dataTransfer.files[0]); }}
          >
            <span className="drop-icon"><FileUp size={27} /></span>
            <strong>{t("import.dropTitle")}</strong>
            <span>{t("import.dropDetail")}</span>
            <span className="button button-secondary button-md">{t("import.choose")}</span>
            <small>{t("import.supported")}</small>
          </button>
        </>
      ) : null}
      <input ref={fileInput} className="sr-only" type="file" accept=".epub,application/epub+zip" onChange={(event) => chooseFile(event.target.files?.[0])} />

      {inspect.isPending ? (
        <Card className="import-scanning" role="status"><LoaderCircle className="spin" size={25} /><strong>{t("import.scanning")}</strong></Card>
      ) : null}

      {draft ? (
        <div className="import-workspace">
          <div className="import-summary card">
            <div className="import-cover">{draft.coverUrl ? <img src={draft.coverUrl} alt="" /> : <BookOpen size={30} />}</div>
            <div><span>{draft.sourceName}</span><strong>{draft.title}</strong><small>{draft.author || t("common.unknown")}</small></div>
            <span className="import-check"><Check size={17} /></span>
          </div>

          {draft.warnings.length ? (
            <Card className="warning-panel"><AlertTriangle size={19} /><div><strong>{t("import.warnings")}</strong>{draft.warnings.map((warning) => <p key={warning}>{warning}</p>)}</div></Card>
          ) : null}

          <Card className="import-section">
            <div className="section-heading"><div><span className="step-number">1</span><h2>{t("import.chapterStep")}</h2><p>{t("import.selectedCount", { selected: selected.size, total: draft.chapters.length })}</p></div><div className="cluster"><Button size="sm" variant="ghost" onClick={() => setSelected(new Set(draft.chapters.map((c) => c.id)))}>{t("import.selectAll")}</Button><Button size="sm" variant="ghost" onClick={() => setSelected(new Set())}>{t("import.selectNone")}</Button></div></div>
            <div className="chapter-picker">
              {draft.chapters.map((chapter) => (
                <label key={chapter.id} className="chapter-pick">
                  <input type="checkbox" checked={selected.has(chapter.id)} onChange={(event) => setSelected((current) => { const next = new Set(current); event.target.checked ? next.add(chapter.id) : next.delete(chapter.id); return next; })} />
                  <span className="chapter-index">{String(chapter.index + 1).padStart(2, "0")}</span>
                  <span><strong>{chapter.title}</strong><small>{t("project.words", { count: chapter.wordCount })}</small></span>
                </label>
              ))}
            </div>
          </Card>

          <Card className="import-section">
            <div className="section-heading"><div><span className="step-number">2</span><h2>{t("import.detailsStep")}</h2></div></div>
            <div className="grid-2 form-grid">
              <Field label={t("import.titleLabel")}><Input value={metadata.title} onChange={(event) => setMetadata({ ...metadata, title: event.target.value })} /></Field>
              <Field label={t("import.authorLabel")}><Input value={metadata.author} onChange={(event) => setMetadata({ ...metadata, author: event.target.value })} /></Field>
              <Field label={t("import.languageLabel")}><Input value={metadata.language} onChange={(event) => setMetadata({ ...metadata, language: event.target.value })} /></Field>
              <Field label={t("import.outputNameLabel")}><Input value={metadata.outputName} onChange={(event) => setMetadata({ ...metadata, outputName: event.target.value })} /></Field>
              <Field label={t("import.seriesLabel")}><Input value={metadata.series} onChange={(event) => setMetadata({ ...metadata, series: event.target.value })} /></Field>
              <Field label={t("import.seriesPositionLabel")}><Input type="number" min="0" step="0.1" value={metadata.seriesPosition} onChange={(event) => setMetadata({ ...metadata, seriesPosition: event.target.value })} /></Field>
            </div>
          </Card>

          <Card className="import-section">
            <div className="section-heading"><div><span className="step-number">3</span><h2>{t("import.consentStep")}</h2><p>{t("import.cloudDetail")}</p></div><ShieldCheck size={22} /></div>
            <div className="stack consent-list">
              <SwitchField checked={cloudText} onCheckedChange={setCloudText} label={t("import.cloudText")} />
              <SwitchField checked={cloudAudio} onCheckedChange={setCloudAudio} label={t("import.cloudAudio")} />
            </div>
          </Card>
          {commit.isError ? <ErrorState error={commit.error} onRetry={() => commit.mutate()} /> : null}
          <div className="import-footer"><Button variant="secondary" onClick={() => setDraft(undefined)}>{t("common.cancel")}</Button><Button size="lg" disabled={!metadata.title.trim() || selected.size === 0 || commit.isPending} onClick={() => commit.mutate()}>{commit.isPending ? <LoaderCircle className="spin" size={17} /> : <Check size={17} />}{commit.isPending ? t("state.saving") : t("import.commit")}</Button></div>
        </div>
      ) : null}
    </div>
  );
}
