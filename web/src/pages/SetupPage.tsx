import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { Activity, ArrowLeft, ArrowRight, Check, Cloud, FolderOpen, KeyRound, Laptop, LoaderCircle, ShieldCheck, Sparkles, Waves } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { useNavigate } from "react-router-dom";
import { api } from "../api/client";
import type { ProviderKind } from "../api/types";
import { ErrorState, LoadingState } from "../components/StateViews";
import { Badge, Button, Card, Field, Input, Select, SwitchField } from "../components/ui";
import { ProviderModelField } from "../providers/ProviderModelField";
import { providerPreset, providerPresetsFor } from "../providers/presets";
import { useProviderModels } from "../providers/useProviderModels";

export function SetupPage() {
  const { t, i18n } = useTranslation();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const settings = useQuery({ queryKey: ["settings"], queryFn: api.settings });
  const desktop = isTauri();
  const [step, setStep] = useState(0);
  const [privacy, setPrivacy] = useState<"local" | "mixed">("local");
  const [providerKind, setProviderKind] = useState<ProviderKind | "">("");
  const [credentialNow, setCredentialNow] = useState(false);
  const [credential, setCredential] = useState("");
  const [endpoint, setEndpoint] = useState("");
  const [model, setModel] = useState("");
  const [storageRoot, setStorageRoot] = useState<string>();
  const [secretPassphrase, setSecretPassphrase] = useState("");
  const [secretPassphraseConfirmation, setSecretPassphraseConfirmation] = useState("");
  const clearProvider = () => {
    setProviderKind("");
    setCredentialNow(false);
    setCredential("");
    setEndpoint("");
    setModel("");
  };
  const choosePrivacy = (value: "local" | "mixed") => {
    if (value !== privacy) {
      clearProvider();
    }
    setPrivacy(value);
  };
  const chooseProvider = (kind: ProviderKind | "") => {
    setProviderKind(kind);
    setCredentialNow(false);
    setCredential("");
    if (kind) {
      const preset = providerPreset(kind);
      setEndpoint(preset.defaultEndpoint);
      setModel(preset.defaultModel);
    } else {
      setEndpoint("");
      setModel("");
    }
  };
  const selectedProvider = providerKind ? providerPreset(providerKind) : undefined;
  const discoveryPreset = selectedProvider ?? providerPreset("native_os");
  const availableModels = useProviderModels({
    enabled: step === 2 && Boolean(selectedProvider),
    name: selectedProvider?.name ?? "",
    kind: discoveryPreset.kind,
    mode: discoveryPreset.defaultMode,
    endpoint,
    credential,
    modelSource: discoveryPreset.modelSource,
  });
  useEffect(() => { if (settings.data?.theme && settings.data.theme !== "system") document.documentElement.dataset.theme = settings.data.theme; }, [settings.data]);
  useEffect(() => {
    if (settings.data && storageRoot === undefined) setStorageRoot(managedStorageRoot(settings.data.libraryPath));
  }, [settings.data, storageRoot]);
  const chooseStorage = useMutation({
    mutationFn: () => invoke<string | null>("choose_storage_directory", { startingDirectory: storageRoot }),
    onSuccess: (selected) => { if (selected) setStorageRoot(selected); },
  });
  const relocateStorage = useMutation({
    mutationFn: async () => {
      const dataRoot = storageRoot?.trim();
      if (!dataRoot) throw new Error(t("setup.storageRootRequired"));
      await invoke("relocate_first_run_storage", { dataRoot });
      return api.settings();
    },
    onSuccess: (value) => {
      queryClient.setQueryData(["settings"], value);
      setStorageRoot(managedStorageRoot(value.libraryPath));
      setStep(4);
    },
  });
  const unlockSecrets = useMutation({
    mutationFn: async () => {
      if (secretPassphrase.length < 12) throw new Error(t("settings.secretPassphraseLength"));
      if (secretPassphrase !== secretPassphraseConfirmation) throw new Error(t("settings.secretPassphraseMismatch"));
      return api.unlockSecretStore(secretPassphrase);
    },
    onSuccess: (status) => {
      queryClient.setQueryData(["settings"], (value: typeof settings.data) => value ? { ...value, secretStore: status.backend } : value);
      setSecretPassphrase("");
      setSecretPassphraseConfirmation("");
    },
  });
  const finish = useMutation({
    mutationFn: async () => {
      await api.updateSettings({ language: i18n.language as "en" | "de" });
      if (selectedProvider && selectedProvider.kind !== "native_os") await api.createProvider({ name: selectedProvider.name, kind: selectedProvider.kind, mode: selectedProvider.defaultMode, endpoint: endpoint || undefined, model: model || undefined, credential: credentialNow && credential ? credential : undefined });
      return api.completeFirstRun();
    },
    onSuccess: async (value) => { queryClient.setQueryData(["settings"], value); await queryClient.invalidateQueries({ queryKey: ["providers"] }); navigate("/library", { replace: true }); },
  });
  if (settings.isLoading) return <div className="setup-shell"><LoadingState label={t("state.loadingSettings")} /></div>;
  if (settings.isError) return <div className="setup-shell"><ErrorState error={settings.error} onRetry={() => void settings.refetch()} /></div>;
  const total = 6;
  const currentStorageRoot = settings.data ? managedStorageRoot(settings.data.libraryPath) : "";
  const previewStorageRoot = storageRoot?.trim() || currentStorageRoot;
  const previewLibraryPath = managedChildPath(previewStorageRoot, "library");
  const previewCachePath = managedChildPath(previewStorageRoot, "cache");
  const storageChanged = normalizedPath(previewStorageRoot) !== normalizedPath(currentStorageRoot);
  const nextDisabled = (step === 3 && (!previewStorageRoot || relocateStorage.isPending)) || (step === 4 && settings.data?.secretStore === "locked");
  const next = () => {
    if (step === 3 && desktop && storageChanged) {
      relocateStorage.mutate();
      return;
    }
    setStep((value) => value + 1);
  };
  return (
    <div className="setup-shell">
      <header className="setup-topbar"><div className="brand"><span className="brand-mark"><i /><i /><i /></span><span>Audiobook<span>AI</span></span></div><div className="language-toggle"><button className={i18n.language === "en" ? "active" : ""} onClick={() => void i18n.changeLanguage("en")}>EN</button><button className={i18n.language === "de" ? "active" : ""} onClick={() => void i18n.changeLanguage("de")}>DE</button></div></header>
      <main className="setup-main">
        <div className="setup-progress"><div className="space-between"><Badge tone="accent">{t("setup.badge")}</Badge><span>{t("setup.steps", { current: step + 1, total })}</span></div><div className="setup-track"><i style={{ width: `${((step + 1) / total) * 100}%` }} /></div></div>
        <section className="setup-panel">
          {step === 0 ? <SetupIntro icon={<Waves size={30} />} title={t("setup.welcomeTitle")} detail={t("setup.welcomeDetail")}><div className="setup-points"><span><Laptop size={18} />{t("shell.localPrivate")}</span><span><KeyRound size={18} />{t("settings.keychain")}</span><span><Sparkles size={18} />{t("characters.title")}</span></div></SetupIntro> : null}
          {step === 1 ? <SetupIntro icon={<ShieldCheck size={30} />} title={t("setup.privacyTitle")} detail={t("setup.privacyDetail")}><div className="choice-grid"><button className={privacy === "local" ? "selected" : ""} onClick={() => choosePrivacy("local")}><Laptop size={23} /><strong>{t("setup.localFirst")}</strong><span>{t("shell.localPrivate")}</span>{privacy === "local" ? <Check size={17} /> : null}</button><button className={privacy === "mixed" ? "selected" : ""} onClick={() => choosePrivacy("mixed")}><Cloud size={23} /><strong>{t("setup.mixed")}</strong><span>{t("import.cloudDetail")}</span>{privacy === "mixed" ? <Check size={17} /> : null}</button></div></SetupIntro> : null}
          {step === 2 ? <SetupIntro icon={<Waves size={30} />} title={t("setup.providerTitle")} detail={t("setup.providerDetail")}><div className="stack setup-form">
            <Field label={t("setup.chooseProvider")}><Select value={providerKind} onChange={(event) => chooseProvider(event.target.value as ProviderKind | "")}>
              <option value="">{t("setup.skip")}</option>
              <optgroup label={t("providers.ttsProviders")}>{providerPresetsFor("tts").filter((provider) => privacy === "mixed" || provider.local).map((provider) => <option key={provider.kind} value={provider.kind}>{provider.name}</option>)}</optgroup>
              <optgroup label={t("providers.llmProviders")}>{providerPresetsFor("llm").filter((provider) => privacy === "mixed" || provider.local).map((provider) => <option key={provider.kind} value={provider.kind}>{provider.name}</option>)}</optgroup>
            </Select></Field>
            {selectedProvider ? <div className={`provider-role-summary provider-role-${selectedProvider.role}`}>
              <span>{selectedProvider.role === "tts" ? <Waves size={18} /> : <Activity size={18} />}</span>
              <div><strong>{t(selectedProvider.role === "tts" ? "providers.ttsProvider" : "providers.llmProvider")}</strong><p>{t(selectedProvider.role === "tts" ? "providers.ttsProviderDetail" : "providers.llmProviderDetail")}</p></div>
              <Badge tone={selectedProvider.role === "tts" ? "accent" : "info"}>{selectedProvider.role.toUpperCase()}</Badge>
            </div> : null}
            {selectedProvider && selectedProvider.defaultMode !== "native" ? <>
              <Field label={t("providers.endpoint")} hint={t("providers.presetEndpointHint")}><Input type="url" value={endpoint} onChange={(event) => setEndpoint(event.target.value)} placeholder={selectedProvider.defaultEndpoint || "https://provider.example/v1"} /></Field>
              <SwitchField checked={credentialNow} onCheckedChange={(enabled) => { setCredentialNow(enabled); if (!enabled) setCredential(""); }} label={credentialNow ? t("setup.keyNow") : t("setup.keyLater")} />
              {credentialNow ? <Field label={t("providers.apiKey")} hint={t("providers.apiKeyPlaceholder")}><Input type="password" autoComplete="new-password" value={credential} onChange={(event) => setCredential(event.target.value)} /></Field> : null}
            </> : null}
            {selectedProvider ? <ProviderModelField role={selectedProvider.role} source={selectedProvider.modelSource} value={model} models={availableModels.models} status={availableModels.status} onChange={setModel} /> : null}
          </div></SetupIntro> : null}
          {step === 3 ? <SetupIntro icon={<FolderOpen size={30} />} title={t("setup.storageTitle")} detail={t("setup.storageDetail")}><div className="stack setup-form">
            <div className="field"><label className="field-label" htmlFor="setup-storage-root">{t("setup.storageRoot")}</label><div className="setup-path-picker"><Input id="setup-storage-root" value={storageRoot ?? ""} readOnly={!desktop} aria-readonly={!desktop} onChange={(event) => setStorageRoot(event.target.value)} />{desktop ? <Button variant="secondary" disabled={chooseStorage.isPending || relocateStorage.isPending} onClick={() => chooseStorage.mutate()}>{chooseStorage.isPending ? <LoaderCircle className="spin" size={16} /> : <FolderOpen size={16} />}{t("setup.browse")}</Button> : null}</div><span className="field-hint">{desktop ? t("setup.storageRootHint") : t("setup.storageDesktopOnly")}</span></div>
            <Field label={t("settings.libraryPath")} hint={t("setup.derivedPathHint")}><Input value={previewLibraryPath} readOnly aria-readonly="true" /></Field>
            <Field label={t("settings.cachePath")} hint={t("setup.derivedPathHint")}><Input value={previewCachePath} readOnly aria-readonly="true" /></Field>
            {chooseStorage.isError ? <ErrorState error={chooseStorage.error} onRetry={() => chooseStorage.mutate()} /> : null}
            {relocateStorage.isError ? <ErrorState error={relocateStorage.error} onRetry={() => relocateStorage.mutate()} /> : null}
          </div></SetupIntro> : null}
          {step === 4 ? <SetupIntro icon={<KeyRound size={30} />} title={t("setup.securityTitle")} detail={t("setup.securityDetail")}>
            <Card className={`setup-security ${settings.data?.secretStore === "locked" ? "is-locked" : ""}`}><span><KeyRound size={20} /></span><div><strong>{t(`settings.${settings.data?.secretStore ?? "locked"}`)}</strong><p>{settings.data?.secretStore === "locked" ? t("settings.secretStoreLockedDetail") : t("settings.secretStoreReadyDetail")}</p></div><Badge tone={settings.data?.secretStore === "locked" ? "warning" : "positive"}>{settings.data?.secretStore === "locked" ? t("settings.secretStoreLocked") : <><Check size={12} />{t("settings.secretStoreReady")}</>}</Badge></Card>
            {settings.data?.secretStore === "locked" ? <div className="stack setup-form setup-passphrase">
              <p>{t("settings.secretPassphraseDetail")}</p>
              <Field label={t("settings.secretPassphrase")}><Input type="password" autoComplete="new-password" value={secretPassphrase} onChange={(event) => setSecretPassphrase(event.target.value)} /></Field>
              <Field label={t("settings.confirmSecretPassphrase")}><Input type="password" autoComplete="new-password" value={secretPassphraseConfirmation} onChange={(event) => setSecretPassphraseConfirmation(event.target.value)} /></Field>
              <Button variant="secondary" disabled={!secretPassphrase || unlockSecrets.isPending} onClick={() => unlockSecrets.mutate()}>{unlockSecrets.isPending ? <LoaderCircle className="spin" size={16} /> : <KeyRound size={16} />}{t("settings.unlockSecretStore")}</Button>
              {unlockSecrets.isError ? <ErrorState error={unlockSecrets.error} onRetry={() => unlockSecrets.mutate()} /> : null}
            </div> : null}
          </SetupIntro> : null}
          {step === 5 ? <SetupIntro icon={<Sparkles size={30} />} title={t("setup.finishTitle")} detail={t("setup.finishDetail")}><div className="setup-summary"><div><span>{t("setup.chooseProvider")}</span><strong>{selectedProvider ? `${selectedProvider.name} · ${selectedProvider.role.toUpperCase()}${model ? ` · ${model}` : ""}` : t("setup.skip")}</strong></div><div><span>{t("settings.libraryPath")}</span><strong>{settings.data?.libraryPath}</strong></div><div><span>{t("settings.cachePath")}</span><strong>{settings.data?.cachePath}</strong></div><div><span>{t("settings.language")}</span><strong>{i18n.language.toUpperCase()}</strong></div></div>{finish.isError ? <ErrorState error={finish.error} /> : null}</SetupIntro> : null}
        </section>
        <footer className="setup-footer"><Button variant="ghost" onClick={() => setStep((value) => Math.max(0, value - 1))} disabled={step === 0 || relocateStorage.isPending}><ArrowLeft size={16} />{t("common.back")}</Button>{step < total - 1 ? <Button size="lg" onClick={next} disabled={nextDisabled}>{relocateStorage.isPending && step === 3 ? <LoaderCircle className="spin" size={17} /> : null}{step === 0 ? t("setup.begin") : relocateStorage.isPending && step === 3 ? t("setup.movingStorage") : t("common.continue")}{relocateStorage.isPending && step === 3 ? null : <ArrowRight size={16} />}</Button> : <Button size="lg" onClick={() => finish.mutate()} disabled={finish.isPending}>{finish.isPending ? <LoaderCircle className="spin" size={17} /> : <Check size={17} />}{t("setup.finish")}</Button>}</footer>
      </main>
    </div>
  );
}

function SetupIntro({ icon, title, detail, children }: { icon: React.ReactNode; title: string; detail: string; children: React.ReactNode }) {
  return <div className="setup-intro"><div className="setup-hero-icon">{icon}</div><h1>{title}</h1><p>{detail}</p><div className="setup-content">{children}</div></div>;
}

function managedStorageRoot(libraryPath: string): string {
  const normalized = libraryPath.replace(/[\\/]+$/, "");
  const slashIndex = normalized.lastIndexOf("/library");
  const backslashIndex = normalized.lastIndexOf("\\library");
  const index = Math.max(slashIndex, backslashIndex);
  if (index < 0 || index + "/library".length !== normalized.length) return "";
  const separator = normalized[index] ?? "/";
  const root = normalized.slice(0, index);
  if (!root) return separator;
  return /^[A-Za-z]:$/.test(root) ? `${root}${separator}` : root;
}

function managedChildPath(root: string, child: "library" | "cache"): string {
  if (!root) return "";
  const separator = root.includes("\\") && !root.includes("/") ? "\\" : "/";
  const withoutTrailingSeparator = root.replace(/[\\/]+$/, "");
  return `${withoutTrailingSeparator}${separator}${child}`;
}

function normalizedPath(path: string): string {
  return path.trim().replace(/[\\/]+$/, "");
}
