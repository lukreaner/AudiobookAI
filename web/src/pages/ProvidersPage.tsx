import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { clsx } from "clsx";
import { Activity, Cloud, Cpu, Download, HardDrive, KeyRound, Laptop, LoaderCircle, MoreHorizontal, PackageOpen, Play, Plus, RefreshCw, RotateCcw, ScrollText, Square, Trash2, Waves, XCircle } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "../api/client";
import type { MlxManagedModel, MlxManagement, ProviderKind, ProviderModel, ProviderProfile, ProviderRole } from "../api/types";
import { EmptyState, ErrorState, LoadingState } from "../components/StateViews";
import { Badge, Button, Card, Dialog, Field, Input, PageHeading, Select, Textarea } from "../components/ui";
import { ProviderModelField } from "../providers/ProviderModelField";
import { PiperManagementCard } from "../providers/PiperManagementCard";
import { providerDefaultsForRole, providerPreset, providerPresetsFor, providerSupportsRole } from "../providers/presets";
import { useProviderModels } from "../providers/useProviderModels";

const statusTone: Record<ProviderProfile["status"], "positive" | "neutral" | "accent" | "warning" | "danger"> = {
  online: "positive", offline: "neutral", starting: "accent", stopping: "warning", error: "danger", unconfigured: "warning",
};

function modeLabel(mode: ProviderProfile["mode"]): string {
  if (mode === "cloud_remote") return "providers.cloud";
  if (mode === "external_endpoint") return "providers.external";
  if (mode === "managed_child") return "providers.managed";
  return "providers.native";
}

type ProviderForm = {
  name: string;
  kind: ProviderKind;
  role: ProviderRole;
  mode: ProviderProfile["mode"];
  endpoint: string;
  executablePath: string;
  workingDirectory: string;
  argumentsText: string;
  credential: string;
  model: string;
};

function providerFormFor(kind: ProviderKind, preferredRole?: ProviderRole, nativeProviderName?: string): ProviderForm {
  const normalizedKind = kind === "openai_tts" ? "openai" : kind;
  const preset = providerPreset(normalizedKind);
  const role = preferredRole && providerSupportsRole(preset, preferredRole)
    ? preferredRole
    : kind === "openai_tts"
      ? "tts"
      : preset.role;
  const roleDefaults = providerDefaultsForRole(preset, role);
  return {
    name: preset.kind === "native_os" ? nativeProviderName ?? preset.name : preset.name,
    kind: preset.kind,
    role,
    mode: preset.defaultMode,
    endpoint: preset.defaultEndpoint,
    executablePath: "",
    workingDirectory: "",
    argumentsText: "",
    credential: "",
    model: roleDefaults.defaultModel,
  };
}

function emptyProviderForm(): ProviderForm {
  return providerFormFor("elevenlabs");
}

function argumentLines(value: string): string[] {
  return value.split(/\r?\n/).filter((argument) => argument.length > 0);
}

function formatBytes(value?: number): string {
  if (value === undefined) return "—";
  if (value < 1024 * 1024) return `${Math.max(1, Math.round(value / 1024))} KiB`;
  if (value < 1024 * 1024 * 1024) return `${(value / (1024 * 1024)).toFixed(1)} MiB`;
  return `${(value / (1024 * 1024 * 1024)).toFixed(2)} GiB`;
}

const installerStatusKeys: Record<MlxManagement["installerStatus"], string> = {
  ready: "providers.mlxInstallerReady",
  unsupported_platform: "providers.mlxInstallerUnsupported",
  not_bundled: "providers.mlxInstallerNotBundled",
  payload_missing: "providers.mlxInstallerPayloadMissing",
  unsafe_filesystem: "providers.mlxInstallerUnsafe",
  invalid_metadata: "providers.mlxInstallerInvalid",
  incomplete: "providers.mlxInstallerIncomplete",
};

export function ProvidersPage() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const providers = useQuery({ queryKey: ["providers"], queryFn: api.providers });
  const nativeAvailability = useQuery({
    queryKey: ["native-provider-availability"],
    queryFn: api.nativeProviderAvailability,
  });
  const mlx = useQuery({
    queryKey: ["mlx-management"],
    queryFn: api.mlxManagement,
    refetchInterval: (query) => query.state.data?.activeOperation ? 750 : false,
  });
  const [editing, setEditing] = useState<ProviderProfile>();
  const [deleting, setDeleting] = useState<ProviderProfile>();
  const [controlling, setControlling] = useState<ProviderProfile>();
  const [modelName, setModelName] = useState("");
  const [downloadModelName, setDownloadModelName] = useState("");
  const [downloadQuantization, setDownloadQuantization] = useState("");
  const [deletingProviderModel, setDeletingProviderModel] = useState<ProviderModel>();
  const [modelRepository, setModelRepository] = useState("");
  const [modelRevision, setModelRevision] = useState("main");
  const [removingModel, setRemovingModel] = useState<MlxManagedModel>();
  const [confirmingUninstall, setConfirmingUninstall] = useState(false);
  const [addOpen, setAddOpen] = useState(false);
  const [form, setForm] = useState<ProviderForm>(emptyProviderForm);
  const selectedPreset = providerPreset(form.kind);
  const selectedRoleDefaults = providerDefaultsForRole(selectedPreset, form.role);
  const availableModels = useProviderModels({
    enabled: addOpen || Boolean(editing),
    providerId: editing?.id,
    credentialConfigured: editing?.credentialConfigured,
    name: form.name,
    kind: form.kind,
    role: form.role,
    mode: form.mode,
    endpoint: form.endpoint,
    executablePath: form.executablePath,
    workingDirectory: form.workingDirectory,
    argumentsText: form.argumentsText,
    credential: form.credential,
    modelSource: selectedRoleDefaults.modelSource,
  });
  const providerModels = useQuery({
    queryKey: ["provider-model-library", controlling?.id],
    queryFn: () => api.providerModels(controlling!.id),
    enabled: Boolean(controlling?.capabilities?.modelList),
    refetchInterval: (query) => query.state.data?.operations.some((operation) => ["running", "cancelling"].includes(operation.state)) ? 750 : false,
  });

  const control = useMutation({
    mutationFn: ({ id, action }: { id: string; action: "start" | "stop" | "restart" | "refresh" }) => api.providerAction(id, action),
    onSuccess: async (provider) => {
      queryClient.setQueryData(["providers"], (current: { items: ProviderProfile[] } | undefined) => current ? { ...current, items: current.items.map((item) => item.id === provider.id ? provider : item) } : current);
      if (provider.kind === "native_os") {
        await queryClient.invalidateQueries({ queryKey: ["native-provider-availability"] });
      }
    },
  });
  const save = useMutation({
    mutationFn: () => {
      const managed = form.mode === "managed_child";
      const shared = {
        name: form.name,
        role: form.role,
        mode: form.mode,
        endpoint: form.mode === "native" ? null : form.endpoint.trim() || null,
        // A legacy native profile may carry a working executable override even when the global
        // Add-provider probe cannot find a system engine. The native editor does not expose that
        // path, so omit these PATCH fields instead of silently clearing a working connection.
        ...(form.mode === "native" && editing ? {} : {
          executablePath: managed ? form.executablePath.trim() || null : null,
          workingDirectory: managed ? form.workingDirectory.trim() || null : null,
        }),
        arguments: managed ? argumentLines(form.argumentsText) : [],
        model: form.model.trim() || null,
        credential: form.credential || undefined,
      };
      return editing
        ? api.updateProvider(editing.id, shared)
        : api.createProvider({ ...shared, name: form.name || selectedPreset.name, kind: form.kind });
    },
    onSuccess: async () => { await queryClient.invalidateQueries({ queryKey: ["providers"] }); closeDialog(); },
  });
  const remove = useMutation({
    mutationFn: (id: string) => api.deleteProvider(id),
    onSuccess: async (_result, deletedId) => {
      setDeleting(undefined);
      queryClient.setQueryData(["providers"], (current: { items: ProviderProfile[]; total: number } | undefined) => {
        if (!current || !current.items.some((provider) => provider.id === deletedId)) return current;
        return {
          ...current,
          items: current.items.filter((provider) => provider.id !== deletedId),
          total: Math.max(0, current.total - 1),
        };
      });
      await queryClient.invalidateQueries({ queryKey: ["providers"] });
    },
  });
  const logs = useMutation({ mutationFn: (id: string) => api.providerLogs(id) });
  const modelControl = useMutation({
    mutationFn: ({ id, action, model }: { id: string; action: "load-model" | "unload-model" | "switch-model"; model: string }) =>
      api.providerModelAction(id, action, model),
    onSuccess: (provider) => {
      setControlling(provider);
      setModelName(provider.model ?? "");
      queryClient.setQueryData(["providers"], (current: { items: ProviderProfile[] } | undefined) =>
        current ? { ...current, items: current.items.map((item) => item.id === provider.id ? provider : item) } : current,
      );
    },
  });
  const refreshProviderModels = async (providerId: string) => {
    await queryClient.invalidateQueries({ queryKey: ["provider-model-library", providerId] });
  };
  const downloadProviderModel = useMutation({
    mutationFn: ({ id, model, quantization }: { id: string; model: string; quantization?: string }) =>
      api.downloadProviderModel(id, model, quantization),
    onSuccess: async (operation) => {
      setDownloadModelName("");
      setDownloadQuantization("");
      await refreshProviderModels(operation.providerProfileId);
    },
  });
  const cancelProviderModel = useMutation({
    mutationFn: ({ id, operationId }: { id: string; operationId: string }) => api.cancelProviderModelDownload(id, operationId),
    onSuccess: async (operation) => refreshProviderModels(operation.providerProfileId),
  });
  const deleteProviderModel = useMutation({
    mutationFn: ({ id, model }: { id: string; model: string }) => api.deleteProviderModel(id, model, true),
    onSuccess: async (_result, variables) => {
      setDeletingProviderModel(undefined);
      await refreshProviderModels(variables.id);
    },
  });
  const refreshMlx = async () => { await queryClient.invalidateQueries({ queryKey: ["mlx-management"] }); };
  const installMlx = useMutation({ mutationFn: api.installMlx, onSuccess: refreshMlx });
  const uninstallMlx = useMutation({
    mutationFn: () => api.uninstallMlx(true),
    onSuccess: async () => { setConfirmingUninstall(false); await refreshMlx(); },
  });
  const cancelMlx = useMutation({ mutationFn: api.cancelMlxOperation, onSuccess: refreshMlx });
  const downloadMlx = useMutation({
    mutationFn: () => api.downloadMlxModel(modelRepository.trim(), modelRevision.trim()),
    onSuccess: async () => { setModelRepository(""); await refreshMlx(); },
  });
  const removeMlx = useMutation({
    mutationFn: (id: string) => api.removeMlxModel(id, true),
    onSuccess: async () => { setRemovingModel(undefined); await refreshMlx(); },
  });
  const selectMlxModel = useMutation({
    mutationFn: ({ providerId, path }: { providerId: string; path: string }) => api.updateProvider(providerId, { model: path }),
    onSuccess: async () => { await queryClient.invalidateQueries({ queryKey: ["providers"] }); },
  });

  const lastMlxOperation = mlx.data?.lastOperation;
  useEffect(() => {
    if (lastMlxOperation?.state === "succeeded") {
      void queryClient.invalidateQueries({ queryKey: ["providers"] });
    }
  }, [lastMlxOperation?.id, lastMlxOperation?.state, queryClient]);

  const lastProviderModelOperation = providerModels.data?.operations[0];
  useEffect(() => {
    if (lastProviderModelOperation?.state === "succeeded" && controlling) {
      void refreshProviderModels(controlling.id);
    }
  }, [lastProviderModelOperation?.id, lastProviderModelOperation?.state]);
  useEffect(() => {
    if (addOpen) void nativeAvailability.refetch();
  }, [addOpen, nativeAvailability.refetch]);

  const closeDialog = () => { setAddOpen(false); setEditing(undefined); setForm(emptyProviderForm()); };
  const openAddForRole = (role: ProviderRole) => {
    setEditing(undefined);
    setForm(providerFormFor(role === "tts" ? "elevenlabs" : "openai", role));
    setAddOpen(true);
  };
  const selectProviderKind = (kind: ProviderKind) => {
    const previous = providerPreset(form.kind);
    const preset = providerPreset(kind);
    const replaceName = !form.name.trim() || form.name === previous.name;
    const role = providerSupportsRole(preset, form.role) ? form.role : preset.role;
    setForm({
      ...providerFormFor(kind, role, nativeAvailability.data?.providerName),
      name: replaceName
        ? preset.kind === "native_os"
          ? nativeAvailability.data?.providerName ?? preset.name
          : preset.name
        : form.name,
    });
  };
  const selectProviderRole = (role: ProviderRole) => {
    if (!providerSupportsRole(selectedPreset, role)) {
      setForm(providerFormFor(role === "tts" ? "elevenlabs" : "openai", role));
      return;
    }
    const defaults = providerDefaultsForRole(selectedPreset, role);
    setForm({ ...form, role, model: defaults.defaultModel });
  };
  const selectProviderMode = (mode: ProviderProfile["mode"]) => {
    setForm({
      ...form,
      mode,
      endpoint: selectedPreset.defaultEndpoint,
      executablePath: mode === "managed_child" ? form.executablePath : "",
      workingDirectory: mode === "managed_child" ? form.workingDirectory : "",
      argumentsText: mode === "managed_child" ? form.argumentsText : "",
      credential: "",
    });
  };
  const configureDeveloperMlx = () => {
    setEditing(undefined);
    setForm({
      ...providerFormFor("mlx_audio"),
      name: "MLX-audio (developer install)",
      kind: "mlx_audio",
      mode: "managed_child",
      endpoint: "http://127.0.0.1:8000/",
      argumentsText: "--host\n127.0.0.1\n--port\n8000",
    });
    setAddOpen(true);
  };
  const openEdit = (provider: ProviderProfile) => { setEditing(provider); setForm({ name: provider.name, kind: provider.kind === "openai_tts" ? "openai" : provider.kind, role: provider.role, mode: provider.mode, endpoint: provider.endpoint ?? "", executablePath: provider.executablePath ?? "", workingDirectory: provider.workingDirectory ?? "", argumentsText: provider.arguments.join("\n"), credential: "", model: provider.model ?? "" }); };
  const requestDelete = (provider: ProviderProfile) => {
    closeDialog();
    remove.reset();
    setDeleting(provider);
  };
  const openControl = (provider: ProviderProfile) => {
    setControlling(provider);
    setModelName(provider.model ?? "");
    logs.reset();
    modelControl.reset();
    downloadProviderModel.reset();
    cancelProviderModel.reset();
    deleteProviderModel.reset();
    if (provider.mode === "managed_child" && provider.status === "online") logs.mutate(provider.id);
  };
  const closeControl = () => {
    setControlling(undefined);
    setModelName("");
    setDownloadModelName("");
    setDownloadQuantization("");
    setDeletingProviderModel(undefined);
    logs.reset();
    modelControl.reset();
    downloadProviderModel.reset();
    cancelProviderModel.reset();
    deleteProviderModel.reset();
  };
  const managed = form.mode === "managed_child";
  const hasEndpoint = form.mode !== "native";
  const managedProcessMayBeOwned = (provider?: ProviderProfile) => provider?.mode === "managed_child" && !["offline", "unconfigured"].includes(provider.status);
  const editBlockedByOwnedProcess = managedProcessMayBeOwned(editing);
  const deleteBlockedByOwnedProcess = managedProcessMayBeOwned(deleting);
  const managedMlxProfiles = providers.data?.items.filter((provider) => provider.kind === "mlx_audio" && provider.mode === "managed_child") ?? [];
  const managedMlxProfile = managedMlxProfiles.length === 1 ? managedMlxProfiles[0] : undefined;
  const activeProviderModelOperation = providerModels.data?.operations.find((operation) => ["running", "cancelling"].includes(operation.state));
  const mlxInstallerUnavailable = Boolean(mlx.data && !mlx.data.installed && mlx.data.installerStatus !== "ready");
  const mlxDeveloperFallback = mlx.data?.supported && ["not_bundled", "payload_missing"].includes(mlx.data.installerStatus);
  const nativeProviderName = nativeAvailability.data?.providerName ?? providerPreset("native_os").name;
  const nativeProviderAvailable = nativeAvailability.data?.available === true;
  const nativeGuidanceKey = nativeAvailability.data?.platform === "linux"
    ? "providers.nativeUnavailableLinux"
    : "providers.nativeUnavailableOther";

  if (providers.isLoading) return <LoadingState label={t("state.loadingProviders")} />;
  if (providers.isError) return <ErrorState error={providers.error} onRetry={() => void providers.refetch()} />;
  return (
    <div className="page providers-page">
      <PageHeading eyebrow={t("providers.eyebrow")} title={t("providers.title")} subtitle={t("providers.subtitle")} actions={<Button onClick={() => setAddOpen(true)}><Plus size={17} />{t("providers.add")}</Button>} />
      <Card className="provider-model-discovery-card">
        <span className="provider-logo"><RefreshCw size={20} /></span>
        <div><strong>{t("providers.modelDiscoveryTitle")}</strong><p>{t("providers.modelDiscoveryOverview")}</p></div>
        <Badge tone="info">{t("providers.automatic")}</Badge>
      </Card>
      <PiperManagementCard providers={providers.data?.items ?? []} onAddConnection={() => { setEditing(undefined); setForm(providerFormFor("piper", "tts")); setAddOpen(true); }} />
      {mlx.isError ? <ErrorState error={mlx.error} onRetry={() => void mlx.refetch()} /> : null}
      {mlx.data ? <Card className="mlx-management-card">
        <div className="mlx-management-head">
          <span className="provider-logo"><PackageOpen size={21} /></span>
          <div><h2>{t("providers.mlxManagerTitle")}</h2><p>{t("providers.mlxManagerDetail")}</p></div>
          <Badge tone={mlx.data.installed ? "positive" : mlxInstallerUnavailable ? "warning" : "neutral"}>{mlx.data.installed ? t("providers.mlxInstalled", { version: mlx.data.installedVersion }) : t(mlxInstallerUnavailable ? "providers.mlxInstallerUnavailableBadge" : "providers.mlxNotInstalled")}</Badge>
        </div>
        <p id="mlx-installer-status" className={mlxInstallerUnavailable ? "provider-form-warning" : "mlx-support-detail"}>{t(installerStatusKeys[mlx.data.installerStatus])} {t("providers.mlxUvRequirement", { version: mlx.data.requiredUvVersion })}</p>
        {mlxDeveloperFallback ? <details className="mlx-developer-setup">
          <summary>{t("providers.mlxDeveloperSetup")}</summary>
          <p>{t("providers.mlxDeveloperSetupDetail")}</p>
        </details> : null}
        {mlx.data.activeOperation ? <div className="mlx-operation" aria-live="polite">
          <div className="space-between"><strong>{mlx.data.activeOperation.message}</strong><span>{mlx.data.activeOperation.progressPercent}%</span></div>
          <progress max={100} value={mlx.data.activeOperation.progressPercent} />
          <Button size="sm" variant="ghost" disabled={cancelMlx.isPending || mlx.data.activeOperation.state === "cancelling"} onClick={() => cancelMlx.mutate(mlx.data!.activeOperation!.id)}><XCircle size={14} />{t("providers.mlxCancel")}</Button>
        </div> : null}
        {mlx.data.lastOperation && mlx.data.lastOperation.state !== "succeeded" ? <p className="provider-form-warning">{mlx.data.lastOperation.message}</p> : null}
        {mlx.data.lastOperation && (mlx.data.lastOperation.diagnostics?.length || mlx.data.lastOperation.exitCode != null) ? <details className="mlx-operation-diagnostics">
          <summary>{t("providers.mlxDiagnostics")}</summary>
          {mlx.data.lastOperation.exitCode != null ? <p>{t("providers.mlxExitCode", { code: mlx.data.lastOperation.exitCode })}</p> : null}
          {mlx.data.lastOperation.diagnostics?.length ? <ul>{mlx.data.lastOperation.diagnostics.map((line) => <li key={line}>{line}</li>)}</ul> : null}
        </details> : null}
        {mlx.data.profileActionRequired ? <p className="provider-form-warning">{t("providers.mlxProfileActionRequired")}</p> : null}
        <div className="mlx-management-actions">
          {!mlx.data.installed ? <Button aria-describedby="mlx-installer-status" disabled={!mlx.data.supported || !mlx.data.installerPayloadAvailable || Boolean(mlx.data.activeOperation) || installMlx.isPending} onClick={() => installMlx.mutate()}>{installMlx.isPending ? <LoaderCircle className="spin" size={16} /> : <Download size={16} />}{t("providers.mlxInstall")}</Button> : null}
          {mlxDeveloperFallback ? <Button variant="secondary" onClick={configureDeveloperMlx}>{t("providers.mlxConfigureExisting")}</Button> : null}
          {mlx.data.installed ? <Button variant="danger" disabled={Boolean(mlx.data.activeOperation) || managedMlxProfiles.length > 0 || uninstallMlx.isPending} onClick={() => setConfirmingUninstall(true)}><Trash2 size={16} />{t("providers.mlxUninstall")}</Button> : null}
          <Button variant="ghost" size="sm" onClick={() => void mlx.refetch()}><RefreshCw size={14} />{t("common.refresh")}</Button>
        </div>
        {mlx.data.installed && managedMlxProfiles.length > 0 ? <p className="mlx-uninstall-note">{t("providers.mlxDeleteProfileFirst")}</p> : null}
        {mlx.data.installed ? <section className="mlx-model-library" aria-labelledby="mlx-model-heading">
          <div><h3 id="mlx-model-heading">{t("providers.mlxModels")}</h3><p>{t("providers.mlxModelsDetail")}</p></div>
          <div className="mlx-download-form">
            <Field label={t("providers.mlxRepository")} hint={t("providers.mlxRepositoryHint")}><Input value={modelRepository} onChange={(event) => setModelRepository(event.target.value)} placeholder="owner/public-model" /></Field>
            <Field label={t("providers.mlxRevision")} hint={t("providers.mlxRevisionHint")}><Input value={modelRevision} onChange={(event) => setModelRevision(event.target.value)} /></Field>
            <Button disabled={!modelRepository.trim() || !modelRevision.trim() || Boolean(mlx.data.activeOperation) || downloadMlx.isPending} onClick={() => downloadMlx.mutate()}>{downloadMlx.isPending ? <LoaderCircle className="spin" size={16} /> : <Download size={16} />}{t("providers.mlxDownloadModel")}</Button>
          </div>
          {mlx.data.models.length ? <div className="mlx-model-list">{mlx.data.models.map((model) => <div key={model.id}>
            <HardDrive size={17} />
            <div><strong>{model.repository}</strong><span>{model.resolvedCommit ? `${model.revision} · ${model.resolvedCommit.slice(0, 12)}` : model.revision} · {formatBytes(model.bytes)}</span><code>{model.localPath}</code></div>
            <div className="card-actions">
              <Button size="sm" variant="secondary" disabled={!managedMlxProfile || model.state !== "ready" || selectMlxModel.isPending} onClick={() => managedMlxProfile && selectMlxModel.mutate({ providerId: managedMlxProfile.id, path: model.localPath })}>{t("providers.mlxUseModel")}</Button>
              <Button aria-label={t("providers.mlxRemoveModel", { name: model.repository })} size="sm" variant="ghost" disabled={model.state === "downloading"} onClick={() => setRemovingModel(model)}><Trash2 size={14} /></Button>
            </div>
          </div>)}</div> : <p className="mlx-empty-models">{t("providers.mlxNoModels")}</p>}
        </section> : null}
        {installMlx.isError ? <ErrorState error={installMlx.error} /> : null}
        {uninstallMlx.isError ? <ErrorState error={uninstallMlx.error} /> : null}
        {cancelMlx.isError ? <ErrorState error={cancelMlx.error} /> : null}
        {downloadMlx.isError ? <ErrorState error={downloadMlx.error} /> : null}
        {selectMlxModel.isError ? <ErrorState error={selectMlxModel.error} /> : null}
      </Card> : null}
      {!providers.data?.items.length ? <EmptyState title={t("providers.emptyTitle")} detail={t("providers.emptyDetail")} action={<Button onClick={() => setAddOpen(true)}><Plus size={16} />{t("providers.add")}</Button>} /> : (
        <div className="provider-role-groups">
          {(["tts", "llm"] as ProviderRole[]).map((role) => {
            const roleProviders = providers.data.items.filter((provider) => provider.role === role);
            return <section className="provider-role-section" aria-labelledby={`provider-role-${role}`} key={role}>
              <header>
                <span>{role === "tts" ? <Waves size={18} /> : <Activity size={18} />}</span>
                <div><h2 id={`provider-role-${role}`}>{t(role === "tts" ? "providers.ttsProviders" : "providers.llmProviders")}</h2><p>{t(role === "tts" ? "providers.ttsProvidersDetail" : "providers.llmProvidersDetail")}</p></div>
                <Badge tone={role === "tts" ? "accent" : "info"}>{role === "tts" ? "TTS" : "LLM"}</Badge>
              </header>
              {roleProviders.length ? <div className="provider-grid">
                {roleProviders.map((provider) => {
                  const ModeIcon = provider.mode === "cloud_remote" ? Cloud : provider.mode === "native" ? Laptop : Cpu;
                  const caps = provider.capabilities;
                  const preset = providerPreset(provider.kind);
                  const roleDefaults = providerDefaultsForRole(preset, provider.role);
                  const isNativeSystemProvider = provider.kind === "native_os";
                  const nativeSetupRequired = isNativeSystemProvider && provider.status === "unconfigured";
                  const nativeProfileGuidanceKey = nativeAvailability.data?.available === false
                    ? nativeGuidanceKey
                    : "providers.nativeProfileNeedsSetup";
                  return (
                    <Card className={clsx("provider-card", (provider.status === "error" || nativeSetupRequired) && "has-error")} key={provider.id}>
                      <div className="provider-card-head">
                        <span className="provider-logo"><ModeIcon size={21} /></span>
                        <div><h2>{provider.name}</h2><p>{t(modeLabel(provider.mode))}</p></div>
                        <Badge tone={statusTone[provider.status]}><span className="service-dot" />{t(`providers.${provider.status}`)}</Badge>
                      </div>
                      <div className="provider-connection">
                        <span>{provider.model || (roleDefaults.modelSource === "none" ? t("providers.systemVoicesNoModelCatalog", { name: isNativeSystemProvider ? nativeProviderName : t("providers.systemVoices") }) : provider.endpoint || t(modeLabel(provider.mode)))}</span>
                        {provider.mode !== "native" ? <span className={provider.credentialConfigured ? "credential-ok" : "credential-missing"}><KeyRound size={13} />{t(provider.credentialConfigured ? "providers.apiKeyConfigured" : "providers.apiKeyMissing")}</span> : null}
                      </div>
                      <div className="capability-list">
                        {caps ? <>
                          {caps.tts ? <Badge tone="accent"><Waves size={12} />{t("providers.tts")}</Badge> : null}
                          {caps.characterDetection ? <Badge tone="info"><Activity size={12} />{t("providers.ai")}</Badge> : null}
                          {caps.streaming ? <Badge>{t("providers.streaming")}</Badge> : null}
                          {caps.voiceCloning ? <Badge>{t("providers.cloning")}</Badge> : null}
                          {caps.modelControl ? <Badge>{t("providers.modelControl")}</Badge> : null}
                        </> : <span className="capability-unknown">{t("providers.capabilityUnknown")}</span>}
                      </div>
                      {nativeSetupRequired ? <div className="provider-error"><span /><div><strong>{t("providers.nativeSetupRequiredTitle", { name: nativeProviderName })}</strong><p>{t(nativeProfileGuidanceKey, { name: nativeProviderName })} {t("providers.nativePiperAlternative")}</p></div></div> : provider.lastError ? <div className="provider-error"><span />{provider.lastError}</div> : null}
                      <div className="provider-actions">
                        {provider.capabilities?.processControl ? <>
                          {provider.status === "online" ? <Button size="sm" variant="secondary" onClick={() => control.mutate({ id: provider.id, action: "stop" })} disabled={control.isPending}><Square size={13} />{t("providers.stop")}</Button> : <Button size="sm" variant="secondary" onClick={() => control.mutate({ id: provider.id, action: "start" })} disabled={control.isPending}><Play size={13} />{t("providers.start")}</Button>}
                          <Button size="sm" variant="ghost" onClick={() => control.mutate({ id: provider.id, action: "restart" })} disabled={control.isPending}><RotateCcw size={14} />{t("providers.restart")}</Button>
                        </> : <Button size="sm" variant="ghost" onClick={() => control.mutate({ id: provider.id, action: "refresh" })} disabled={control.isPending}><RefreshCw size={14} />{t("providers.refresh")}</Button>}
                        {provider.capabilities?.processControl || provider.capabilities?.modelControl ? <Button size="sm" variant="ghost" onClick={() => openControl(provider)}><ScrollText size={14} />{t("providers.control")}</Button> : null}
                        <Button className="provider-config" size="sm" variant="ghost" onClick={() => openEdit(provider)}><MoreHorizontal size={15} />{t("common.settings")}</Button>
                      </div>
                    </Card>
                  );
                })}
              </div> : <div className="provider-role-empty">
                <p>{t(role === "tts" ? "providers.noTtsProviders" : "providers.noLlmProviders")}</p>
                <Button size="sm" variant="secondary" onClick={() => openAddForRole(role)}><Plus size={14} />{t(role === "tts" ? "providers.addTtsProvider" : "providers.addLlmProvider")}</Button>
              </div>}
            </section>;
          })}
        </div>
      )}
      {control.isError ? <ErrorState error={control.error} onRetry={() => control.reset()} /> : null}

      <Dialog open={addOpen || Boolean(editing)} onOpenChange={(open) => !open && closeDialog()} title={editing ? t("providers.configure", { name: editing.name }) : t("providers.add")} description={t("providers.subtitle")} size="lg" footer={<>{editing ? <Button variant="danger" onClick={() => requestDelete(editing)}><Trash2 size={16} />{t("providers.delete")}</Button> : null}<Button variant="secondary" onClick={closeDialog}>{t("common.cancel")}</Button><Button disabled={!form.name.trim() || (managed && !form.executablePath.trim()) || (form.kind === "piper" && !form.model.trim()) || (form.kind === "native_os" && !nativeProviderAvailable && (!editing || editing.status === "unconfigured")) || editBlockedByOwnedProcess || save.isPending} onClick={() => save.mutate()}>{save.isPending ? <LoaderCircle className="spin" size={16} /> : <RefreshCw size={16} />}{t("providers.saveAndCheck")}</Button></>}>
        <div className="provider-form stack">
          {editBlockedByOwnedProcess ? <p className="provider-form-warning">{t("providers.stopBeforeConfigure")}</p> : null}
          <div className={`provider-role-summary provider-role-${form.role}`}>
            <span>{form.role === "tts" ? <Waves size={18} /> : <Activity size={18} />}</span>
            <div><strong>{t(form.role === "tts" ? "providers.ttsProvider" : "providers.llmProvider")}</strong><p>{t(form.role === "tts" ? "providers.ttsProviderDetail" : "providers.llmProviderDetail")}</p></div>
            <Badge tone={form.role === "tts" ? "accent" : "info"}>{form.role.toUpperCase()}</Badge>
          </div>
          <div className="grid-2">
            <Field label={t("providers.role")} hint={t("providers.roleHint")}><Select value={form.role} onChange={(event) => selectProviderRole(event.target.value as ProviderRole)}><option value="tts" disabled={Boolean(editing) && !providerSupportsRole(selectedPreset, "tts")}>{t("providers.roleTts")}</option><option value="llm" disabled={Boolean(editing) && !providerSupportsRole(selectedPreset, "llm")}>{t("providers.roleLlm")}</option></Select></Field>
            <Field label={t("providers.chooseType")}><Select value={form.kind} onChange={(event) => selectProviderKind(event.target.value as ProviderKind)} disabled={Boolean(editing)}>{editing && selectedPreset.hidden ? <option value={selectedPreset.kind}>{selectedPreset.name}</option> : null}{providerPresetsFor(form.role).map((preset) => {
              const native = preset.kind === "native_os";
              const label = native ? nativeProviderName : preset.name;
              const nativeOptionLabel = nativeAvailability.isLoading
                ? t("providers.nativeCheckingOption", { name: label })
                : nativeAvailability.data?.available
                  ? label
                  : t("providers.nativeSetupOption", { name: label });
              return <option value={preset.kind} key={preset.kind} disabled={native && !nativeProviderAvailable}>{native ? nativeOptionLabel : label}</option>;
            })}</Select></Field>
          </div>
          {form.role === "tts" && !nativeProviderAvailable && !nativeAvailability.isLoading ? <details className="native-provider-setup-note"><summary>{nativeAvailability.isError ? t("providers.nativeAvailabilityFailedShort") : t("providers.nativeSetupOption", { name: nativeProviderName })}</summary><div className="native-provider-warning" role="status"><strong>{nativeAvailability.isError ? t("providers.nativeAvailabilityFailed") : t("providers.nativeUnavailableTitle", { name: nativeProviderName })}</strong>{nativeAvailability.data ? <><span>{t(nativeGuidanceKey, { name: nativeProviderName })}</span><span>{t("providers.nativePiperAlternative")}</span></> : null}<Button size="sm" variant="ghost" disabled={nativeAvailability.isFetching} onClick={() => void nativeAvailability.refetch()}><RefreshCw className={nativeAvailability.isFetching ? "spin" : undefined} size={14} />{t("providers.nativeCheckAgain")}</Button></div></details> : null}
          <Field label={t("providers.mode")}><Select value={form.mode} onChange={(event) => selectProviderMode(event.target.value as ProviderProfile["mode"])}>{selectedPreset.modes.map((mode) => <option value={mode} key={mode}>{t(modeLabel(mode))}</option>)}</Select></Field>
          <Field label={t("import.titleLabel")}><Input value={form.name} onChange={(event) => setForm({ ...form, name: event.target.value })} /></Field>
          {hasEndpoint ? <Field label={t("providers.endpoint")} hint={managed ? t("providers.managedEndpointHint") : t("providers.presetEndpointHint")}><Input type="url" value={form.endpoint} onChange={(event) => setForm({ ...form, endpoint: event.target.value })} placeholder={selectedPreset.defaultEndpoint || "https://provider.example/v1"} /></Field> : null}
          {managed ? <Field label={t("providers.executable")} hint={t("providers.executableHint")}><Input value={form.executablePath} onChange={(event) => setForm({ ...form, executablePath: event.target.value })} /></Field> : null}
          {managed ? <div className="grid-2">
            <Field label={t("providers.workingDirectory")} hint={t("providers.workingDirectoryHint")}><Input value={form.workingDirectory} onChange={(event) => setForm({ ...form, workingDirectory: event.target.value })} /></Field>
            <Field label={t("providers.arguments")} hint={t("providers.argumentsHint")}><Textarea rows={4} spellCheck={false} value={form.argumentsText} onChange={(event) => setForm({ ...form, argumentsText: event.target.value })} /></Field>
          </div> : null}
          {managed ? <p className="provider-form-note">{t("providers.environmentSecurity")}</p> : null}
          <div className="grid-2">
            {form.mode !== "native" ? <Field label={t("providers.apiKey")} hint={editing?.credentialConfigured ? t("providers.apiKeyConfigured") : t("providers.apiKeyPlaceholder")}><Input type="password" autoComplete="new-password" value={form.credential} onChange={(event) => setForm({ ...form, credential: event.target.value })} placeholder="••••••••••••" /></Field> : null}
            <ProviderModelField role={form.role} source={selectedRoleDefaults.modelSource} value={form.model} models={availableModels.models} status={availableModels.status} strict={availableModels.strict} onChange={(model) => setForm((current) => ({ ...current, model }))} />
          </div>
          {save.isError ? <ErrorState error={save.error} /> : null}
        </div>
      </Dialog>

      <Dialog
        open={Boolean(deleting)}
        onOpenChange={(open) => { if (!open) { setDeleting(undefined); remove.reset(); } }}
        title={t("providers.deleteTitle", { name: deleting?.name ?? "" })}
        description={t(deleteBlockedByOwnedProcess ? "providers.deleteRunningDetail" : deleting?.kind === "native_os" ? "providers.deleteNativeDetail" : "providers.deleteDetail")}
        footer={<><Button variant="secondary" onClick={() => { setDeleting(undefined); remove.reset(); }}>{t("common.cancel")}</Button><Button variant="danger" disabled={!deleting || deleteBlockedByOwnedProcess || remove.isPending} onClick={() => deleting && remove.mutate(deleting.id)}>{remove.isPending ? <LoaderCircle className="spin" size={16} /> : <Trash2 size={16} />}{t("providers.deleteConfirm")}</Button></>}
      >
        {remove.isError ? <ErrorState error={remove.error} /> : <p>{t(deleting?.kind === "native_os" ? "providers.deleteNativeOwnershipNote" : "providers.deleteOwnershipNote")}</p>}
      </Dialog>

      <Dialog
        open={confirmingUninstall}
        onOpenChange={(open) => { setConfirmingUninstall(open); if (!open) uninstallMlx.reset(); }}
        title={t("providers.mlxUninstallTitle")}
        description={t("providers.mlxUninstallDetail")}
        footer={<><Button variant="secondary" onClick={() => setConfirmingUninstall(false)}>{t("common.cancel")}</Button><Button variant="danger" disabled={uninstallMlx.isPending} onClick={() => uninstallMlx.mutate()}>{uninstallMlx.isPending ? <LoaderCircle className="spin" size={16} /> : <Trash2 size={16} />}{t("providers.mlxConfirmUninstall")}</Button></>}
      >
        {uninstallMlx.isError ? <ErrorState error={uninstallMlx.error} /> : <p>{t("providers.mlxUninstallSafety")}</p>}
      </Dialog>

      <Dialog
        open={Boolean(removingModel)}
        onOpenChange={(open) => { if (!open) { setRemovingModel(undefined); removeMlx.reset(); } }}
        title={t("providers.mlxRemoveModelTitle", { name: removingModel?.repository ?? "" })}
        description={t("providers.mlxRemoveModelDetail")}
        footer={<><Button variant="secondary" onClick={() => setRemovingModel(undefined)}>{t("common.cancel")}</Button><Button variant="danger" disabled={!removingModel || removeMlx.isPending} onClick={() => removingModel && removeMlx.mutate(removingModel.id)}>{removeMlx.isPending ? <LoaderCircle className="spin" size={16} /> : <Trash2 size={16} />}{t("providers.mlxConfirmRemove")}</Button></>}
      >
        {removeMlx.isError ? <ErrorState error={removeMlx.error} /> : <p>{t("providers.mlxRemoveOwnershipNote")}</p>}
      </Dialog>

      <Dialog
        open={Boolean(controlling)}
        onOpenChange={(open) => !open && closeControl()}
        title={t("providers.controlTitle", { name: controlling?.name ?? "" })}
        description={t("providers.controlDetail")}
        size="lg"
        footer={<Button variant="secondary" onClick={closeControl}>{t("common.close")}</Button>}
      >
        {controlling ? <div className="stack provider-control-panel">
          {controlling.capabilities && (controlling.capabilities.modelLoad || controlling.capabilities.modelUnload || controlling.capabilities.modelSwitch) ? <section className="stack provider-model-control" aria-labelledby="provider-model-heading">
            <div>
              <h3 id="provider-model-heading">{t("providers.modelControl")}</h3>
              <p>{t("providers.modelControlDetail")}</p>
            </div>
            <Field label={t("providers.modelName")} hint={t("providers.modelNameHint")}>
              <Input value={modelName} onChange={(event) => setModelName(event.target.value)} />
            </Field>
            <div className="provider-control-actions">
              {controlling.capabilities.modelLoad ? <Button size="sm" disabled={!modelName.trim() || modelControl.isPending} onClick={() => modelControl.mutate({ id: controlling.id, action: "load-model", model: modelName.trim() })}>{t("providers.loadModel")}</Button> : null}
              {controlling.capabilities.modelSwitch ? <Button size="sm" variant="secondary" disabled={!modelName.trim() || modelControl.isPending} onClick={() => modelControl.mutate({ id: controlling.id, action: "switch-model", model: modelName.trim() })}>{t("providers.switchModel")}</Button> : null}
              {controlling.capabilities.modelUnload ? <Button size="sm" variant="ghost" disabled={!(modelName.trim() || controlling.model) || modelControl.isPending} onClick={() => modelControl.mutate({ id: controlling.id, action: "unload-model", model: modelName.trim() || controlling.model || "" })}>{t("providers.unloadModel")}</Button> : null}
            </div>
            {modelControl.isError ? <ErrorState error={modelControl.error} onRetry={() => modelControl.reset()} /> : null}
          </section> : null}
          {controlling.capabilities?.modelList ? <section className="stack provider-model-library" aria-labelledby="provider-library-heading">
            <div className="provider-log-heading">
              <div><h3 id="provider-library-heading">{t("providers.modelLibrary")}</h3><p>{t(controlling.kind === "ollama" ? "providers.ollamaModelLibraryDetail" : controlling.kind === "localai" ? "providers.localAiModelLibraryDetail" : "providers.lmStudioModelLibraryDetail")}</p></div>
              <Button size="sm" variant="ghost" onClick={() => void providerModels.refetch()} disabled={providerModels.isFetching}>{providerModels.isFetching ? <LoaderCircle className="spin" size={14} /> : <RefreshCw size={14} />}{t("common.refresh")}</Button>
            </div>
            {activeProviderModelOperation ? <div className="provider-model-operation" aria-live="polite">
              <div className="space-between"><strong>{t(`providers.modelOperation_${activeProviderModelOperation.state}`)}</strong><span>{activeProviderModelOperation.progressPercent === undefined ? "—" : `${activeProviderModelOperation.progressPercent}%`}</span></div>
              <progress max={100} value={activeProviderModelOperation.progressPercent} />
              <div className="space-between"><span>{formatBytes(activeProviderModelOperation.downloadedBytes)} / {formatBytes(activeProviderModelOperation.totalSizeBytes)}</span><Button size="sm" variant="ghost" disabled={activeProviderModelOperation.state === "cancelling" || cancelProviderModel.isPending} onClick={() => cancelProviderModel.mutate({ id: controlling.id, operationId: activeProviderModelOperation.id })}><XCircle size={14} />{t("providers.cancelModelDownload")}</Button></div>
              <p>{t("providers.modelCancellationDetail")}</p>
            </div> : null}
            {!activeProviderModelOperation && lastProviderModelOperation ? <div className="provider-model-operation" aria-live="polite">
              <div className="space-between"><strong>{t(`providers.modelOperation_${lastProviderModelOperation.state}`)}</strong><span>{lastProviderModelOperation.progressPercent === undefined ? "—" : `${lastProviderModelOperation.progressPercent}%`}</span></div>
            </div> : null}
            {controlling.capabilities.modelDownload ? <div className="provider-model-download-form">
              <Field label={t("providers.downloadModelIdentifier")} hint={t(controlling.kind === "ollama" ? "providers.ollamaDownloadHint" : controlling.kind === "localai" ? "providers.localAiDownloadHint" : "providers.lmStudioDownloadHint")}><Input value={downloadModelName} onChange={(event) => setDownloadModelName(event.target.value)} /></Field>
              {controlling.kind === "lm_studio" ? <Field label={t("providers.downloadQuantization")} hint={t("providers.downloadQuantizationHint")}><Input value={downloadQuantization} onChange={(event) => setDownloadQuantization(event.target.value)} placeholder="Q4_K_M" /></Field> : null}
              <Button disabled={!downloadModelName.trim() || Boolean(activeProviderModelOperation) || downloadProviderModel.isPending} onClick={() => downloadProviderModel.mutate({ id: controlling.id, model: downloadModelName.trim(), quantization: downloadQuantization.trim() || undefined })}>{downloadProviderModel.isPending ? <LoaderCircle className="spin" size={16} /> : <Download size={16} />}{t("providers.downloadModel")}</Button>
            </div> : null}
            {providerModels.isLoading ? <LoadingState label={t("providers.loadingModels")} /> : null}
            {providerModels.isError ? <ErrorState error={providerModels.error} onRetry={() => void providerModels.refetch()} /> : null}
            {providerModels.data?.modelsErrorCode ? <p className="provider-form-warning">{t("providers.modelListUnavailable")}</p> : null}
            {providerModels.data ? providerModels.data.models.length ? <div className="provider-model-list">{providerModels.data.models.map((model) => {
              const loaded = model.loadedInstances.length > 0;
              const selected = controlling.model === model.id;
              return <div key={model.id}>
                <HardDrive size={17} />
                <div><strong>{model.name}</strong><span>{[model.parameterSize, model.quantization, model.format, formatBytes(model.sizeBytes)].filter((value) => value && value !== "—").join(" · ")}</span><code>{model.id}</code></div>
                <div className="card-actions">
                  {loaded ? <Badge tone="positive">{t("providers.modelLoaded")}</Badge> : null}
                  <Button size="sm" variant="secondary" onClick={() => setModelName(model.loadedInstances[0] || model.id)}>{t("providers.selectModel")}</Button>
                  {controlling.capabilities?.modelDelete ? <Button aria-label={t("providers.deleteInstalledModel", { name: model.name })} size="sm" variant="ghost" disabled={loaded || selected} onClick={() => setDeletingProviderModel(model)}><Trash2 size={14} /></Button> : null}
                </div>
              </div>;
            })}</div> : <p className="mlx-empty-models">{t("providers.noProviderModels")}</p> : null}
            {downloadProviderModel.isError ? <ErrorState error={downloadProviderModel.error} /> : null}
            {cancelProviderModel.isError ? <ErrorState error={cancelProviderModel.error} /> : null}
          </section> : null}
          {controlling.capabilities?.processControl ? <section className="stack provider-log-section" aria-labelledby="provider-log-heading">
            <div className="provider-log-heading">
              <div><h3 id="provider-log-heading">{t("providers.logs")}</h3><p>{t("providers.logsDetail")}</p></div>
              <Button size="sm" variant="ghost" onClick={() => logs.mutate(controlling.id)} disabled={logs.isPending}>{logs.isPending ? <LoaderCircle className="spin" size={14} /> : <RefreshCw size={14} />}{t("common.refresh")}</Button>
            </div>
            {logs.isError ? <ErrorState error={logs.error} onRetry={() => logs.mutate(controlling.id)} /> : null}
            {logs.data ? <div className="provider-log-output" role="log" aria-live="polite">
              {logs.data.logs.length ? logs.data.logs.map((entry, index) => <div className={entry.stream === "stderr" ? "log-stderr" : undefined} key={`${entry.timestamp}-${index}`}><time dateTime={entry.timestamp}>{new Date(entry.timestamp).toLocaleTimeString()}</time><span>{entry.stream}</span><code>{entry.line}</code></div>) : <p>{t("providers.noLogs")}</p>}
            </div> : null}
          </section> : null}
        </div> : null}
      </Dialog>

      <Dialog
        open={Boolean(deletingProviderModel)}
        onOpenChange={(open) => { if (!open) { setDeletingProviderModel(undefined); deleteProviderModel.reset(); } }}
        title={t("providers.deleteInstalledModelTitle", { name: deletingProviderModel?.name ?? "" })}
        description={t("providers.deleteInstalledModelDetail")}
        footer={<><Button variant="secondary" onClick={() => setDeletingProviderModel(undefined)}>{t("common.cancel")}</Button><Button variant="danger" disabled={!controlling || !deletingProviderModel || deleteProviderModel.isPending} onClick={() => controlling && deletingProviderModel && deleteProviderModel.mutate({ id: controlling.id, model: deletingProviderModel.id })}>{deleteProviderModel.isPending ? <LoaderCircle className="spin" size={16} /> : <Trash2 size={16} />}{t("providers.confirmDeleteInstalledModel")}</Button></>}
      >
        {deleteProviderModel.isError ? <ErrorState error={deleteProviderModel.error} /> : <p>{t("providers.deleteInstalledModelSafety")}</p>}
      </Dialog>
    </div>
  );
}
