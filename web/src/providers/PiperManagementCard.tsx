import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertTriangle, Download, HardDrive, LoaderCircle, PackageOpen, Plus, RefreshCw, Trash2, Volume2, XCircle } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "../api/client";
import type { PiperCatalogVoice, PiperInstalledVoice, ProviderProfile } from "../api/types";
import { ErrorState } from "../components/StateViews";
import { Badge, Button, Card, Dialog, Field, Select } from "../components/ui";

function formatBytes(value?: number): string {
  if (value === undefined) return "—";
  if (value < 1024 * 1024) return `${Math.max(1, Math.round(value / 1024))} KiB`;
  if (value < 1024 * 1024 * 1024) return `${(value / (1024 * 1024)).toFixed(1)} MiB`;
  return `${(value / (1024 * 1024 * 1024)).toFixed(2)} GiB`;
}

export function PiperManagementCard({ providers, onAddConnection }: { providers: ProviderProfile[]; onAddConnection: () => void }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const piper = useQuery({
    queryKey: ["piper-management"],
    queryFn: api.piperManagement,
    refetchInterval: (query) => query.state.data?.activeOperation ? 750 : false,
  });
  const [confirmingUninstall, setConfirmingUninstall] = useState(false);
  const [downloadingVoice, setDownloadingVoice] = useState<PiperCatalogVoice>();
  const [licenseConfirmed, setLicenseConfirmed] = useState(false);
  const [removingVoice, setRemovingVoice] = useState<Pick<PiperInstalledVoice, "id" | "name">>();
  const [choosingVoice, setChoosingVoice] = useState<PiperInstalledVoice>();
  const [profileId, setProfileId] = useState("");
  const piperProfiles = providers.filter((provider) => provider.kind === "piper" && provider.role === "tts");

  const refresh = async (refreshProviders = false) => {
    const requests = [queryClient.invalidateQueries({ queryKey: ["piper-management"] })];
    if (refreshProviders) requests.push(queryClient.invalidateQueries({ queryKey: ["providers"] }));
    await Promise.all(requests);
  };
  const install = useMutation({ mutationFn: api.installPiper, onSuccess: () => refresh(true) });
  const uninstall = useMutation({
    mutationFn: () => api.uninstallPiper(true),
    onSuccess: async () => { setConfirmingUninstall(false); await refresh(true); },
  });
  const cancel = useMutation({ mutationFn: api.cancelPiperOperation, onSuccess: () => refresh(true) });
  const download = useMutation({
    mutationFn: (voiceId: string) => api.downloadPiperVoice(voiceId, true),
    onSuccess: async () => {
      setDownloadingVoice(undefined);
      setLicenseConfirmed(false);
      await refresh(true);
    },
  });
  const remove = useMutation({
    mutationFn: (voiceId: string) => api.removePiperVoice(voiceId, true),
    onSuccess: async () => { setRemovingVoice(undefined); await refresh(true); },
  });
  const selectVoice = useMutation({
    mutationFn: ({ providerId, voiceId }: { providerId: string; voiceId: string }) => api.updateProvider(providerId, { model: voiceId }),
    onSuccess: async () => {
      setChoosingVoice(undefined);
      setProfileId("");
      await refresh(true);
    },
  });

  const lastOperation = piper.data?.lastOperation;
  useEffect(() => {
    if (lastOperation?.state === "succeeded") void queryClient.invalidateQueries({ queryKey: ["providers"] });
  }, [lastOperation?.id, lastOperation?.state, queryClient]);

  const useVoice = (voice: PiperInstalledVoice) => {
    if (piperProfiles.length === 0) {
      onAddConnection();
      return;
    }
    if (piperProfiles.length === 1) {
      selectVoice.mutate({ providerId: piperProfiles[0].id, voiceId: voice.id });
      return;
    }
    const current = piperProfiles.find((provider) => provider.model === voice.id);
    setProfileId(current?.id ?? piperProfiles[0].id);
    setChoosingVoice(voice);
  };

  if (piper.isLoading) return null;
  if (piper.isError) return <ErrorState error={piper.error} onRetry={() => void piper.refetch()} />;
  if (!piper.data) return null;

  const management = piper.data;
  const installedIds = new Set(management.installedVoices.map((voice) => voice.id));
  const issueIds = new Set(management.voiceIssues.map((issue) => issue.id));
  const operationBusy = Boolean(management.activeOperation);
  const installingAvailable = management.supported && management.installerStatus === "ready";
  const incompleteInstall = management.supported && management.installerStatus === "incomplete";
  const mutationError = install.error ?? uninstall.error ?? cancel.error ?? download.error ?? remove.error ?? selectVoice.error;

  return <>
    <Card className="piper-management-card">
      <div className="piper-management-head">
        <span className="provider-logo"><Volume2 size={21} /></span>
        <div><h2>{t("providers.piperManagerTitle")}</h2><p>{t("providers.piperManagerDetail")}</p></div>
        <Badge tone={management.installed ? "positive" : installingAvailable ? "neutral" : "warning"}>
          {management.installed
            ? t("providers.piperInstalled", { version: management.installedVersion ?? t("providers.piperReady") })
            : t(installingAvailable ? "providers.piperNotInstalled" : "providers.piperUnavailable")}
        </Badge>
      </div>
      <p className={management.supported ? "piper-support-detail" : "provider-form-warning"}>{management.supportDetail}</p>

      {management.activeOperation ? <div className="piper-operation" aria-live="polite">
        <div className="space-between"><strong>{management.activeOperation.message}</strong><span>{management.activeOperation.progressPercent}%</span></div>
        <progress max={100} value={management.activeOperation.progressPercent} />
        {management.activeOperation.bytesTotal ? <p>{t("providers.piperDownloadProgress", { downloaded: formatBytes(management.activeOperation.bytesDownloaded), total: formatBytes(management.activeOperation.bytesTotal) })}</p> : null}
        <Button size="sm" variant="ghost" disabled={cancel.isPending || management.activeOperation.state === "cancelling"} onClick={() => cancel.mutate(management.activeOperation!.id)}><XCircle size={14} />{t("providers.piperCancel")}</Button>
      </div> : null}
      {management.lastOperation && ["failed", "cancelled"].includes(management.lastOperation.state) ? <p className="provider-form-warning">{management.lastOperation.message}</p> : null}

      <div className="piper-management-actions">
        {!management.installed ? <Button disabled={!installingAvailable || operationBusy || install.isPending} onClick={() => install.mutate()}>{install.isPending ? <LoaderCircle className="spin" size={16} /> : <Download size={16} />}{t("providers.piperInstall")}</Button> : null}
        {management.installed || incompleteInstall ? <Button variant="danger" disabled={operationBusy || piperProfiles.length > 0 || uninstall.isPending} onClick={() => setConfirmingUninstall(true)}><Trash2 size={16} />{t("providers.piperUninstall")}</Button> : null}
        {management.installed && piperProfiles.length === 0 ? <Button variant="secondary" disabled={operationBusy} onClick={onAddConnection}><Plus size={16} />{t("providers.piperAddConnection")}</Button> : null}
        <Button variant="ghost" size="sm" disabled={piper.isFetching} onClick={() => void piper.refetch()}><RefreshCw size={14} />{t("common.refresh")}</Button>
      </div>
      {management.installed && piperProfiles.length > 0 ? <p className="piper-uninstall-note">{t("providers.piperDeleteProfilesFirst")}</p> : null}

      {management.installed && (management.executablePath || management.voicesDir) ? <details className="piper-install-details">
        <summary>{t("providers.piperInstallDetails")}</summary>
        {management.executablePath ? <p><span>{t("providers.piperProgram")}</span><code>{management.executablePath}</code></p> : null}
        {management.voicesDir ? <p><span>{t("providers.piperVoiceStorage")}</span><code>{management.voicesDir}</code></p> : null}
      </details> : null}

      {management.supported ? <section className="piper-voice-library" aria-labelledby="piper-catalog-heading">
        <div><h3 id="piper-catalog-heading">{t("providers.piperCatalogTitle")}</h3><p>{t("providers.piperCatalogDetail")}</p></div>
        {management.catalog.length ? <div className="piper-voice-list">{management.catalog.map((voice) => {
          const installed = installedIds.has(voice.id);
          return <div key={voice.id}>
            <PackageOpen size={17} />
            <div>
              <strong>{voice.name}</strong>
              <span>{voice.language} · {voice.quality} · {formatBytes(voice.sizeBytes)}</span>
              <a href={voice.licenseUrl} target="_blank" rel="noreferrer">{t("providers.piperLicense", { license: voice.license })}</a>
            </div>
            {installed
              ? <Badge tone="positive">{t("providers.piperVoiceInstalled")}</Badge>
              : issueIds.has(voice.id)
                ? <Badge tone="warning">{t("providers.piperVoiceNeedsAttention")}</Badge>
              : <Button size="sm" disabled={!management.installed || operationBusy || download.isPending} onClick={() => { setDownloadingVoice(voice); setLicenseConfirmed(false); }}>{t("providers.piperDownloadVoice")}</Button>}
          </div>;
        })}</div> : <p className="piper-empty-voices">{t("providers.piperCatalogEmpty")}</p>}
      </section> : null}

      {management.voiceIssues.length ? <section className="piper-voice-library" aria-labelledby="piper-issues-heading">
        <div><h3 id="piper-issues-heading">{t("providers.piperVoiceIssues")}</h3><p>{t("providers.piperVoiceIssuesDetail")}</p></div>
        <div className="piper-voice-list piper-voice-issues">{management.voiceIssues.map((issue) => {
          const name = management.catalog.find((voice) => voice.id === issue.id)?.name ?? issue.id;
          return <div key={issue.id}>
            <AlertTriangle size={17} />
            <div>
              <strong>{name}</strong>
              <span>{issue.detail}</span>
              <small>{t(issue.removable ? "providers.piperVoiceIssueRecoverable" : "providers.piperVoiceIssueManual")}</small>
            </div>
            {issue.removable
              ? <Button size="sm" variant="secondary" disabled={operationBusy || remove.isPending} onClick={() => setRemovingVoice({ id: issue.id, name })}>{t("providers.piperRemoveIncompleteVoice")}</Button>
              : <Badge tone="warning">{t("providers.piperManualResolution")}</Badge>}
          </div>;
        })}</div>
      </section> : null}

      {management.installed ? <section className="piper-voice-library" aria-labelledby="piper-installed-heading">
        <div><h3 id="piper-installed-heading">{t("providers.piperInstalledVoices")}</h3><p>{t("providers.piperInstalledVoicesDetail")}</p></div>
        {(management.profileActionRequired || piperProfiles.length === 0) && management.installedVoices.length ? <p className="provider-form-warning">{t("providers.piperProfileActionRequired")}</p> : null}
        {management.installedVoices.length ? <div className="piper-voice-list">{management.installedVoices.map((voice) => {
          const usedBy = piperProfiles.filter((provider) => provider.model === voice.id);
          return <div key={voice.id}>
            <HardDrive size={17} />
            <div>
              <strong>{voice.name}</strong>
              <span>{voice.language} · {voice.quality} · {formatBytes(voice.sizeBytes)} · {voice.license}</span>
              {usedBy.length ? <small>{t("providers.piperUsedBy", { names: usedBy.map((provider) => provider.name).join(", ") })}</small> : null}
            </div>
            <div className="card-actions">
              <Button size="sm" variant="secondary" disabled={operationBusy || selectVoice.isPending} onClick={() => useVoice(voice)}>{piperProfiles.length ? t("providers.piperUseVoice") : t("providers.piperAddConnection")}</Button>
              <Button aria-label={t("providers.piperRemoveVoice", { name: voice.name })} size="sm" variant="ghost" disabled={operationBusy || usedBy.length > 0 || remove.isPending} title={usedBy.length ? t("providers.piperRemoveInUse") : undefined} onClick={() => setRemovingVoice(voice)}><Trash2 size={14} /></Button>
            </div>
          </div>;
        })}</div> : <p className="piper-empty-voices">{t("providers.piperNoInstalledVoices")}</p>}
      </section> : null}

      {mutationError ? <ErrorState error={mutationError} /> : null}
    </Card>

    <Dialog
      open={Boolean(downloadingVoice)}
      onOpenChange={(open) => { if (!open) { setDownloadingVoice(undefined); setLicenseConfirmed(false); download.reset(); } }}
      title={t("providers.piperDownloadTitle", { name: downloadingVoice?.name ?? "" })}
      description={t("providers.piperDownloadDetail")}
      footer={<><Button variant="secondary" onClick={() => { setDownloadingVoice(undefined); setLicenseConfirmed(false); }}>{t("common.cancel")}</Button><Button disabled={!downloadingVoice || !licenseConfirmed || download.isPending} onClick={() => downloadingVoice && download.mutate(downloadingVoice.id)}>{download.isPending ? <LoaderCircle className="spin" size={16} /> : <Download size={16} />}{t("providers.piperAcceptAndDownload")}</Button></>}
    >
      {downloadingVoice ? <div className="stack">
        <p>{downloadingVoice.language} · {downloadingVoice.quality} · {formatBytes(downloadingVoice.sizeBytes)}</p>
        <p>{t("providers.piperLicense", { license: downloadingVoice.license })}</p>
        <p>{t("providers.piperLicenseSummary")}</p>
        <p><a href={downloadingVoice.modelCardUrl} target="_blank" rel="noreferrer">{t("providers.piperReadModelCard")}</a> · <a href={downloadingVoice.licenseUrl} target="_blank" rel="noreferrer">{t("providers.piperReadLicense")}</a> · <a href={downloadingVoice.sourceUrl} target="_blank" rel="noreferrer">{t("providers.piperReadSource")}</a></p>
        <label className="piper-license-confirm"><input type="checkbox" checked={licenseConfirmed} onChange={(event) => setLicenseConfirmed(event.target.checked)} />{t("providers.piperLicenseConfirm")}</label>
      </div> : null}
    </Dialog>

    <Dialog
      open={Boolean(removingVoice)}
      onOpenChange={(open) => { if (!open) { setRemovingVoice(undefined); remove.reset(); } }}
      title={t("providers.piperRemoveTitle", { name: removingVoice?.name ?? "" })}
      description={t("providers.piperRemoveDetail")}
      footer={<><Button variant="secondary" onClick={() => setRemovingVoice(undefined)}>{t("common.cancel")}</Button><Button variant="danger" disabled={!removingVoice || remove.isPending} onClick={() => removingVoice && remove.mutate(removingVoice.id)}>{remove.isPending ? <LoaderCircle className="spin" size={16} /> : <Trash2 size={16} />}{t("providers.piperConfirmRemove")}</Button></>}
    ><p>{t("providers.piperRemoveSafety")}</p></Dialog>

    <Dialog
      open={confirmingUninstall}
      onOpenChange={(open) => { setConfirmingUninstall(open); if (!open) uninstall.reset(); }}
      title={t("providers.piperUninstallTitle")}
      description={t("providers.piperUninstallDetail")}
      footer={<><Button variant="secondary" onClick={() => setConfirmingUninstall(false)}>{t("common.cancel")}</Button><Button variant="danger" disabled={uninstall.isPending} onClick={() => uninstall.mutate()}>{uninstall.isPending ? <LoaderCircle className="spin" size={16} /> : <Trash2 size={16} />}{t("providers.piperConfirmUninstall")}</Button></>}
    ><p>{t("providers.piperUninstallSafety")}</p></Dialog>

    <Dialog
      open={Boolean(choosingVoice)}
      onOpenChange={(open) => { if (!open) { setChoosingVoice(undefined); setProfileId(""); selectVoice.reset(); } }}
      title={t("providers.piperUseVoiceTitle", { name: choosingVoice?.name ?? "" })}
      description={t("providers.piperUseVoiceDetail")}
      footer={<><Button variant="secondary" onClick={() => { setChoosingVoice(undefined); setProfileId(""); }}>{t("common.cancel")}</Button><Button disabled={!choosingVoice || !profileId || selectVoice.isPending} onClick={() => choosingVoice && selectVoice.mutate({ providerId: profileId, voiceId: choosingVoice.id })}>{selectVoice.isPending ? <LoaderCircle className="spin" size={16} /> : <Volume2 size={16} />}{t("providers.piperUseVoice")}</Button></>}
    >
      <Field label={t("providers.piperConnection")}><Select value={profileId} onChange={(event) => setProfileId(event.target.value)}>{piperProfiles.map((provider) => <option value={provider.id} key={provider.id}>{provider.name}</option>)}</Select></Field>
    </Dialog>
  </>;
}
