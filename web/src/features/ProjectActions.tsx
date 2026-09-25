import { useMutation, useQueryClient } from "@tanstack/react-query";
import { LoaderCircle, Pencil, Save, Trash2 } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useNavigate } from "react-router-dom";
import { api } from "../api/client";
import type { ProjectDetail } from "../api/types";
import { ErrorState } from "../components/StateViews";
import { Button, Dialog, Field, Input, SwitchField, Textarea } from "../components/ui";
import { forgetDetectionSettings } from "./detectionControls";
import { forgetExportSettings } from "./exportSettings";

interface DetailsForm {
  title: string;
  author: string;
  narrator: string;
  language: string;
  series: string;
  seriesPosition: string;
  outputName: string;
  description: string;
}

function detailsForm(project: ProjectDetail): DetailsForm {
  return {
    title: project.title,
    author: project.author ?? "",
    narrator: project.narrator ?? "",
    language: project.language ?? "",
    series: project.series ?? "",
    seriesPosition: project.seriesPosition == null ? "" : String(project.seriesPosition),
    outputName: project.outputName ?? "",
    description: project.description ?? "",
  };
}

/** An emptied optional field is cleared rather than left unchanged. */
const optional = (value: string) => value.trim() || null;

export function ProjectActions({ project }: { project: ProjectDetail }) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [editing, setEditing] = useState(false);
  const [form, setForm] = useState<DetailsForm>(() => detailsForm(project));
  const [deleting, setDeleting] = useState(false);
  const [deleteConfirmed, setDeleteConfirmed] = useState(false);
  const seriesPosition = form.seriesPosition.trim() ? Number(form.seriesPosition) : null;
  const seriesPositionInvalid = seriesPosition !== null && !(Number.isFinite(seriesPosition) && seriesPosition >= 0);

  const save = useMutation({
    mutationFn: () => api.updateProject(project.id, {
      title: form.title.trim(),
      author: optional(form.author),
      narrator: optional(form.narrator),
      language: optional(form.language),
      series: optional(form.series),
      seriesPosition,
      outputName: optional(form.outputName),
      description: optional(form.description),
    }),
    onSuccess: async (updated) => {
      queryClient.setQueryData(["project", project.id], updated);
      await queryClient.invalidateQueries({ queryKey: ["projects"] });
      setEditing(false);
    },
  });
  const remove = useMutation({
    mutationFn: () => api.deleteProject(project.id),
    onSuccess: async () => {
      forgetDetectionSettings(project.id);
      forgetExportSettings(project.id);
      queryClient.removeQueries({ queryKey: ["project", project.id] });
      await queryClient.invalidateQueries({ queryKey: ["projects"] });
      navigate("/library", { replace: true });
    },
  });
  const field = (key: keyof DetailsForm) => ({
    value: form[key],
    disabled: save.isPending,
    onChange: (event: { target: { value: string } }) => setForm((current) => ({ ...current, [key]: event.target.value })),
  });

  return (
    <div className="cluster project-actions">
      <Button size="sm" variant="secondary" onClick={() => { setForm(detailsForm(project)); save.reset(); setEditing(true); }}><Pencil size={15} />{t("project.editDetails")}</Button>
      <Button size="sm" variant="ghost" onClick={() => { setDeleteConfirmed(false); remove.reset(); setDeleting(true); }}><Trash2 size={15} />{t("project.delete")}</Button>

      <Dialog
        open={editing}
        onOpenChange={(open) => { if (!save.isPending) setEditing(open); }}
        title={t("project.metadata")}
        description={t("project.editDetailsDetail")}
        size="lg"
        footer={<>
          <Button variant="secondary" disabled={save.isPending} onClick={() => setEditing(false)}>{t("common.cancel")}</Button>
          <Button disabled={!form.title.trim() || seriesPositionInvalid || save.isPending} onClick={() => save.mutate()}>{save.isPending ? <LoaderCircle className="spin" size={16} /> : <Save size={16} />}{save.isPending ? t("state.saving") : t("common.save")}</Button>
        </>}
      >
        <form className="stack" onSubmit={(event) => { event.preventDefault(); if (form.title.trim() && !seriesPositionInvalid) save.mutate(); }}>
          <div className="grid-2 form-grid">
            <Field label={t("import.titleLabel")} error={form.title.trim() ? undefined : t("project.titleRequired")}><Input autoFocus {...field("title")} /></Field>
            <Field label={t("import.authorLabel")}><Input {...field("author")} /></Field>
            <Field label={t("project.narratorLabel")}><Input {...field("narrator")} /></Field>
            <Field label={t("import.languageLabel")} hint={t("project.languageHint")}><Input {...field("language")} /></Field>
            <Field label={t("import.seriesLabel")}><Input {...field("series")} /></Field>
            <Field label={t("import.seriesPositionLabel")} error={seriesPositionInvalid ? t("project.seriesPositionInvalid") : undefined}><Input type="number" min="0" step="0.1" {...field("seriesPosition")} /></Field>
            <Field label={t("import.outputNameLabel")} hint={t("project.outputNameHint")} className="form-span-2"><Input {...field("outputName")} /></Field>
            <Field label={t("project.descriptionLabel")} className="form-span-2"><Textarea {...field("description")} /></Field>
          </div>
          {/* Lets Enter in a single-line field save the form. */}
          <button type="submit" hidden />
          {save.isError ? <ErrorState error={save.error} /> : null}
        </form>
      </Dialog>

      <Dialog
        open={deleting}
        onOpenChange={(open) => { if (!remove.isPending) setDeleting(open); }}
        title={t("project.deleteTitle", { title: project.title })}
        description={t("project.deleteDetail")}
        size="sm"
        footer={<>
          <Button variant="secondary" disabled={remove.isPending} onClick={() => setDeleting(false)}>{t("common.cancel")}</Button>
          <Button variant="danger" disabled={!deleteConfirmed || remove.isPending} onClick={() => remove.mutate()}>{remove.isPending ? <LoaderCircle className="spin" size={16} /> : <Trash2 size={16} />}{t("project.deleteAction")}</Button>
        </>}
      >
        <div className="stack">
          <SwitchField checked={deleteConfirmed} disabled={remove.isPending} onCheckedChange={setDeleteConfirmed} label={t("project.deleteConfirm")} detail={t("project.deleteConfirmDetail")} />
          {remove.isError ? <ErrorState error={remove.error} /> : null}
        </div>
      </Dialog>
    </div>
  );
}
