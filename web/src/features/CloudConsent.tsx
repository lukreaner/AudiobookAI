import { useMutation, useQueryClient } from "@tanstack/react-query";
import { CloudUpload } from "lucide-react";
import { useTranslation } from "react-i18next";
import { api } from "../api/client";
import type { ProjectDetail } from "../api/types";
import { ErrorState } from "../components/StateViews";
import { Card, SwitchField } from "../components/ui";

type ConsentPatch = { consentCloudText?: boolean; consentCloudAudio?: boolean };

function useCloudConsent(projectId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (patch: ConsentPatch) => api.updateProject(projectId, patch),
    onSuccess: (project: ProjectDetail) => {
      queryClient.setQueryData(["project", projectId], project);
    },
  });
}

/** The project's cloud permissions, changeable at any time after import. */
export function CloudConsentCard({ project }: { project: ProjectDetail }) {
  const { t } = useTranslation();
  const consent = useCloudConsent(project.id);
  return (
    <Card className="cloud-consent-card">
      <div className="section-heading">
        <div><h2>{t("project.cloudConsentTitle")}</h2><p>{t("project.cloudConsentDetail")}</p></div>
      </div>
      <div className="stack">
        <SwitchField
          checked={project.consentCloudText}
          disabled={consent.isPending}
          onCheckedChange={(allowed) => consent.mutate({ consentCloudText: allowed })}
          label={t("import.cloudText")}
          detail={t("project.cloudTextConsentDetail")}
        />
        <SwitchField
          checked={project.consentCloudAudio}
          disabled={consent.isPending}
          onCheckedChange={(allowed) => consent.mutate({ consentCloudAudio: allowed })}
          label={t("characters.cloudAudioConsent")}
          detail={t("characters.cloudAudioConsentDetail")}
        />
      </div>
      {consent.isError ? <ErrorState error={consent.error} /> : null}
    </Card>
  );
}

/** Shown where book text would go to a cloud provider that the project does not yet permit. */
export function CloudTextConsentNotice({ projectId, providerName }: { projectId: string; providerName: string }) {
  const { t } = useTranslation();
  const consent = useCloudConsent(projectId);
  return (
    <div className="provider-form-warning cloud-consent-notice" role="status">
      <strong><CloudUpload size={15} />{t("characters.cloudTextRequiredTitle", { name: providerName })}</strong>
      <span>{t("characters.cloudTextRequiredDetail", { name: providerName })}</span>
      <SwitchField
        checked={false}
        disabled={consent.isPending}
        onCheckedChange={(allowed) => { if (allowed) consent.mutate({ consentCloudText: true }); }}
        label={t("import.cloudText")}
      />
      {consent.isError ? <ErrorState error={consent.error} /> : null}
    </div>
  );
}
