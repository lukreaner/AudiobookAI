import * as Tabs from "@radix-ui/react-tabs";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { isTauri } from "@tauri-apps/api/core";
import type { Update } from "@tauri-apps/plugin-updater";
import { Check, Copy, Database, Download, HardDrive, KeyRound, LoaderCircle, LockKeyhole, MonitorCog, Plus, RefreshCw, ShieldCheck, SlidersHorizontal, Trash2, Volume2, Wifi } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "../api/client";
import type { AppSettings, IssuedLanApiToken } from "../api/types";
import { ErrorState, LoadingState } from "../components/StateViews";
import { Badge, Button, Card, Field, Input, PageHeading, ProgressBar, Select, SwitchField } from "../components/ui";
import { activeJobCount } from "../features/updatePolicy";
import { formatBytes } from "../lib/format";

export function SettingsPage() {
  const { t, i18n } = useTranslation();
  const queryClient = useQueryClient();
  const settings = useQuery({ queryKey: ["settings"], queryFn: api.settings });
  const [form, setForm] = useState<AppSettings>();
  const [saved, setSaved] = useState(false);
  const [pendingUpdate, setPendingUpdate] = useState<Update | null>();
  const [updateProgress, setUpdateProgress] = useState<{ downloaded: number; total?: number }>();
  const [lanPassword, setLanPassword] = useState("");
  const [lanPasswordConfirmation, setLanPasswordConfirmation] = useState("");
  const [tokenLabel, setTokenLabel] = useState("");
  const [issuedToken, setIssuedToken] = useState<IssuedLanApiToken>();
  const [tokenCopied, setTokenCopied] = useState(false);
  const [secretPassphrase, setSecretPassphrase] = useState("");
  const [secretPassphraseConfirmation, setSecretPassphraseConfirmation] = useState("");
  const desktop = isTauri();
  useEffect(() => { if (settings.data) setForm(settings.data); }, [settings.data]);
  const lanTokens = useQuery({ queryKey: ["settings", "lan", "tokens"], queryFn: api.lanTokens });
  const save = useMutation({
    mutationFn: () => api.updateSettings(form ? editableSettingsPatch(form, settings.data) : {}),
    onSuccess: (value) => { queryClient.setQueryData(["settings"], value); setForm(value); setSaved(true); window.setTimeout(() => setSaved(false), 2400); },
  });
  const revoke = useMutation({ mutationFn: api.revokeLanSessions, onSuccess: () => setForm((value) => value ? { ...value, lan: { ...value.lan, activeSessions: 0 } } : value) });
  const unlockSecrets = useMutation({
    mutationFn: async () => {
      if (secretPassphrase.length < 12) throw new Error(t("settings.secretPassphraseLength"));
      if (secretPassphrase !== secretPassphraseConfirmation) throw new Error(t("settings.secretPassphraseMismatch"));
      return api.unlockSecretStore(secretPassphrase);
    },
    onSuccess: (status) => {
      queryClient.setQueryData<AppSettings>(["settings"], (value) => value ? { ...value, secretStore: status.backend } : value);
      setForm((value) => value ? { ...value, secretStore: status.backend } : value);
      setSecretPassphrase("");
      setSecretPassphraseConfirmation("");
    },
  });
  const lockSecrets = useMutation({
    mutationFn: api.lockSecretStore,
    onSuccess: () => {
      queryClient.setQueryData<AppSettings>(["settings"], (value) => value ? { ...value, secretStore: "locked" } : value);
      setForm((value) => value ? { ...value, secretStore: "locked" } : value);
    },
  });
  const saveLanPassword = useMutation({
    mutationFn: async () => {
      if (lanPassword.length < 12) throw new Error(t("settings.passwordLength"));
      if (lanPassword !== lanPasswordConfirmation) throw new Error(t("settings.passwordMismatch"));
      await api.setLanPassword(lanPassword);
    },
    onSuccess: async () => {
      setLanPassword("");
      setLanPasswordConfirmation("");
      await settings.refetch();
    },
  });
  const createToken = useMutation({
    gcTime: 0,
    mutationFn: () => api.createLanToken(tokenLabel),
    onSuccess: async (token) => {
      setIssuedToken(token);
      setTokenCopied(false);
      setTokenLabel("");
      await Promise.all([lanTokens.refetch(), settings.refetch()]);
    },
  });
  const revokeToken = useMutation({
    mutationFn: api.revokeLanToken,
    onSuccess: async () => {
      await Promise.all([lanTokens.refetch(), settings.refetch()]);
    },
  });
  const checkUpdate = useMutation({
    mutationFn: async () => {
      if (!desktop) throw new Error(t("settings.updateDesktopOnly"));
      const { check } = await import("@tauri-apps/plugin-updater");
      return check({ timeout: 30_000 });
    },
    onSuccess: (update) => {
      setPendingUpdate(update);
      setUpdateProgress(undefined);
    },
  });
  const installUpdate = useMutation({
    mutationFn: async () => {
      if (!pendingUpdate) throw new Error(t("settings.updateMissing"));
      const jobs = await api.jobs();
      const blockingJobs = activeJobCount(jobs.items);
      if (blockingJobs) {
        throw new Error(t("settings.updateJobsActive", { count: blockingJobs }));
      }
      setUpdateProgress({ downloaded: 0 });
      await pendingUpdate.downloadAndInstall((event) => {
        if (event.event === "Started") {
          setUpdateProgress({ downloaded: 0, total: event.data.contentLength ?? undefined });
        } else if (event.event === "Progress") {
          setUpdateProgress((current) => ({
            downloaded: (current?.downloaded ?? 0) + event.data.chunkLength,
            total: current?.total,
          }));
        } else {
          setUpdateProgress((current) => ({
            downloaded: current?.total ?? current?.downloaded ?? 1,
            total: current?.total ?? current?.downloaded ?? 1,
          }));
        }
      });
      const { relaunch } = await import("@tauri-apps/plugin-process");
      await relaunch();
    },
  });
  if (settings.isLoading) return <LoadingState label={t("state.loadingSettings")} />;
  if (settings.isError) return <ErrorState error={settings.error} onRetry={() => void settings.refetch()} />;
  if (!form) return <LoadingState label={t("state.loadingSettings")} />;

  const patch = <K extends keyof AppSettings>(key: K, value: AppSettings[K]) => setForm({ ...form, [key]: value });
  const patchLan = <K extends keyof AppSettings["lan"]>(key: K, value: AppSettings["lan"][K]) => setForm({ ...form, lan: { ...form.lan, [key]: value } });
  return (
    <div className="page settings-page">
      <PageHeading eyebrow={t("settings.eyebrow")} title={t("settings.title")} subtitle={t("settings.subtitle")} actions={<Button onClick={() => save.mutate()} disabled={save.isPending}>{save.isPending ? <LoaderCircle className="spin" size={16} /> : saved ? <Check size={16} /> : null}{saved ? t("settings.saved") : t("common.save")}</Button>} />
      {save.isError ? <ErrorState error={save.error} onRetry={() => save.mutate()} /> : null}
      <Tabs.Root className="settings-layout" defaultValue="general" orientation="vertical">
        <Tabs.List className="settings-nav" aria-label={t("settings.title")}>
          <Tabs.Trigger value="general"><SlidersHorizontal size={17} />{t("settings.general")}</Tabs.Trigger>
          <Tabs.Trigger value="audio"><Volume2 size={17} />{t("settings.audio")}</Tabs.Trigger>
          <Tabs.Trigger value="storage"><HardDrive size={17} />{t("settings.storage")}</Tabs.Trigger>
          <Tabs.Trigger value="security"><ShieldCheck size={17} />{t("settings.security")}</Tabs.Trigger>
          <Tabs.Trigger value="updates"><RefreshCw size={17} />{t("settings.updates")}</Tabs.Trigger>
        </Tabs.List>
        <div className="settings-content">
          <Tabs.Content value="general"><SettingsSection icon={<MonitorCog size={20} />} title={t("settings.general")}><div className="grid-2"><Field label={t("settings.language")}><Select value={form.language} onChange={(event) => { const language = event.target.value as AppSettings["language"]; patch("language", language); void i18n.changeLanguage(language); }}><option value="en">{t("settings.languageEnglish")}</option><option value="de">{t("settings.languageGerman")}</option></Select></Field><Field label={t("settings.theme")}><Select value={form.theme} onChange={(event) => { const theme = event.target.value as AppSettings["theme"]; patch("theme", theme); if (theme === "system") document.documentElement.removeAttribute("data-theme"); else document.documentElement.dataset.theme = theme; }}><option value="system">{t("settings.themeSystem")}</option><option value="light">{t("settings.themeLight")}</option><option value="dark">{t("settings.themeDark")}</option></Select></Field></div><SwitchField checked={form.closeToTray} onCheckedChange={(value) => patch("closeToTray", value)} label={t("settings.closeToTray")} /></SettingsSection></Tabs.Content>
          <Tabs.Content value="audio"><SettingsSection icon={<Volume2 size={20} />} title={t("settings.audio")} description={t("settings.audioDefaultsDetail")}><div className="grid-2"><Field label={t("settings.loudness")} hint={t("settings.loudnessHint")}><Input type="number" min="-30" max="-10" step="0.5" value={form.defaultLufs} onChange={(event) => patch("defaultLufs", Number(event.target.value))} /></Field><Field label={t("settings.truePeak")} hint={t("settings.truePeakHint")}><Input type="number" min="-10" max="0" step="0.5" value={form.defaultTruePeakDb} onChange={(event) => patch("defaultTruePeakDb", Number(event.target.value))} /></Field><Field label={t("settings.concurrency")} hint={t("settings.newProjectsHint")}><Input type="number" min="1" max="32" value={form.defaultConcurrency} onChange={(event) => patch("defaultConcurrency", Number(event.target.value))} /></Field><Field label={t("settings.retries")} hint={t("settings.newProjectsHint")}><Input type="number" min="0" max="10" value={form.defaultRetryCount} onChange={(event) => patch("defaultRetryCount", Number(event.target.value))} /></Field></div></SettingsSection></Tabs.Content>
          <Tabs.Content value="storage"><SettingsSection icon={<Database size={20} />} title={t("settings.storage")} description={t("settings.managedStorageDetail")}><div className="stack"><Field label={t("settings.libraryPath")} hint={t("settings.managedPathHint")}><Input value={form.libraryPath} readOnly aria-readonly="true" /></Field><Field label={t("settings.cachePath")} hint={t("settings.managedPathHint")}><Input value={form.cachePath} readOnly aria-readonly="true" /></Field><Field label={t("settings.cacheLimit")} hint={t("settings.cacheLimitHint", { value: formatBytes(form.cacheLimitBytes, i18n.language) })}><Input type="number" min="1" step="1" value={Math.round(form.cacheLimitBytes / 1_000_000_000)} onChange={(event) => patch("cacheLimitBytes", Number(event.target.value) * 1_000_000_000)} /></Field></div></SettingsSection></Tabs.Content>
          <Tabs.Content value="security">
            <SettingsSection icon={<KeyRound size={20} />} title={t("settings.security")}>
              <div className={`secret-status ${form.secretStore === "locked" ? "is-locked" : ""}`}><span><LockKeyhole size={19} /></span><div><strong>{t(`settings.${form.secretStore}`)}</strong><p>{form.secretStore === "locked" ? t("settings.secretStoreLockedDetail") : t("settings.secretStoreReadyDetail")}</p></div><Badge tone={form.secretStore === "locked" ? "warning" : "positive"}>{form.secretStore === "locked" ? t("settings.secretStoreLocked") : t("settings.secretStoreReady")}</Badge></div>
              {form.secretStore === "locked" ? <div className="secret-controls">
                <p>{t("settings.secretPassphraseDetail")}</p>
                <div className="grid-2">
                  <Field label={t("settings.secretPassphrase")}><Input type="password" autoComplete="current-password" value={secretPassphrase} onChange={(event) => setSecretPassphrase(event.target.value)} /></Field>
                  <Field label={t("settings.confirmSecretPassphrase")}><Input type="password" autoComplete="current-password" value={secretPassphraseConfirmation} onChange={(event) => setSecretPassphraseConfirmation(event.target.value)} /></Field>
                </div>
                <Button size="sm" variant="secondary" disabled={!secretPassphrase || unlockSecrets.isPending} onClick={() => unlockSecrets.mutate()}>{unlockSecrets.isPending ? <LoaderCircle className="spin" size={15} /> : <KeyRound size={15} />}{t("settings.unlockSecretStore")}</Button>
                {unlockSecrets.isError ? <ErrorState error={unlockSecrets.error} onRetry={() => unlockSecrets.mutate()} /> : null}
              </div> : null}
              {form.secretStore === "passphrase" ? <div className="secret-controls">
                <p>{t("settings.lockSecretStoreDetail")}</p>
                <Button size="sm" variant="secondary" disabled={lockSecrets.isPending} onClick={() => lockSecrets.mutate()}>{lockSecrets.isPending ? <LoaderCircle className="spin" size={15} /> : <LockKeyhole size={15} />}{t("settings.lockSecretStore")}</Button>
                {lockSecrets.isError ? <ErrorState error={lockSecrets.error} onRetry={() => lockSecrets.mutate()} /> : null}
              </div> : null}
            </SettingsSection>
            <SettingsSection icon={<Wifi size={20} />} title={t("settings.lanMode")} description={t("settings.lanDetail")}>
              <SwitchField checked={form.lan.enabled} onCheckedChange={(value) => patchLan("enabled", value)} label={t("settings.lanMode")} detail={t("settings.lanDetail")} />
              <div className="lan-auth-status">
                <Badge tone={form.lan.passwordConfigured ? "positive" : "warning"}>{form.lan.passwordConfigured ? t("settings.passwordConfigured") : t("settings.passwordMissing")}</Badge>
                <Badge tone={form.lan.apiTokenCount ? "positive" : "neutral"}>{t("settings.apiTokenCount", { count: form.lan.apiTokenCount })}</Badge>
                {form.lan.restartRequired ? <Badge tone="warning">{t("settings.restartRequired")}</Badge> : null}
              </div>
              {form.lan.enabled ? <div className="lan-settings">
                <div className="grid-2">
                  <Field label={t("settings.bindAddress")} hint={t("settings.bindAddressHint")}><Input value={form.lan.bindAddress} onChange={(event) => patchLan("bindAddress", event.target.value)} placeholder="0.0.0.0" /></Field>
                  <Field label={t("settings.port")}><Input type="number" min="1" max="65535" value={form.lan.port} onChange={(event) => patchLan("port", Number(event.target.value))} /></Field>
                </div>
                <Field label={t("settings.advertisedHosts")} hint={t("settings.advertisedHostsHint")}><Input value={form.lan.advertisedHosts.join(", ")} onChange={(event) => patchLan("advertisedHosts", parseHosts(event.target.value))} placeholder="reader.home.arpa" /></Field>
                <SwitchField checked={form.lan.tls} onCheckedChange={(value) => setForm({ ...form, lan: { ...form.lan, tls: value, insecureHttpConfirmed: value ? false : form.lan.insecureHttpConfirmed } })} label={t("settings.tls")} detail={t("settings.tlsDetail")} />
                {form.lan.tls ? <>
                  <Field label={t("settings.certificatePath")} hint={t("settings.absolutePemPath")}><Input value={form.lan.certificateChainPath} onChange={(event) => patchLan("certificateChainPath", event.target.value)} placeholder="/absolute/path/fullchain.pem" /></Field>
                  <Field label={t("settings.privateKeyPath")} hint={t("settings.absolutePemPath")}><Input type="password" autoComplete="off" value={form.lan.privateKeyPath} onChange={(event) => patchLan("privateKeyPath", event.target.value)} placeholder="/absolute/path/privkey.pem" /></Field>
                  <p className="settings-note">{t("settings.tlsTrustDetail")}</p>
                </> : <Card className="insecure-warning"><ShieldCheck size={19} /><div><strong>{t("settings.insecureTitle")}</strong><p>{t("settings.insecureWarning")}</p></div><SwitchField checked={form.lan.insecureHttpConfirmed} onCheckedChange={(value) => patchLan("insecureHttpConfirmed", value)} label={t("settings.insecureConfirm")} /></Card>}
                <p className="settings-note">{t("settings.restartDetail")}</p>
              </div> : null}
              <div className="session-row"><span>{t("settings.activeSessions", { count: form.lan.activeSessions })}</span><Button size="sm" variant="secondary" disabled={!form.lan.activeSessions || revoke.isPending} onClick={() => revoke.mutate()}>{t("settings.revokeSessions")}</Button></div>
              <div className="lan-credential-block">
                <h3>{t("settings.browserPassword")}</h3>
                <p>{t("settings.browserPasswordDetail")}</p>
                <div className="grid-2">
                  <Field label={t("settings.newPassword")}><Input type="password" autoComplete="new-password" value={lanPassword} onChange={(event) => setLanPassword(event.target.value)} /></Field>
                  <Field label={t("settings.confirmPassword")}><Input type="password" autoComplete="new-password" value={lanPasswordConfirmation} onChange={(event) => setLanPasswordConfirmation(event.target.value)} /></Field>
                </div>
                <Button size="sm" variant="secondary" disabled={!lanPassword || saveLanPassword.isPending} onClick={() => saveLanPassword.mutate()}>{saveLanPassword.isPending ? <LoaderCircle className="spin" size={15} /> : <KeyRound size={15} />}{form.lan.passwordConfigured ? t("settings.replacePassword") : t("settings.setPassword")}</Button>
                {saveLanPassword.isError ? <ErrorState error={saveLanPassword.error} onRetry={() => saveLanPassword.mutate()} /> : null}
              </div>
              <div className="lan-credential-block">
                <h3>{t("settings.apiTokens")}</h3>
                <p>{t("settings.apiTokensDetail")}</p>
                <div className="lan-token-create"><Field label={t("settings.tokenLabel")}><Input maxLength={80} value={tokenLabel} onChange={(event) => setTokenLabel(event.target.value)} placeholder={t("settings.tokenLabelPlaceholder")} /></Field><Button size="sm" variant="secondary" disabled={!tokenLabel.trim() || createToken.isPending} onClick={() => createToken.mutate()}>{createToken.isPending ? <LoaderCircle className="spin" size={15} /> : <Plus size={15} />}{t("settings.createToken")}</Button></div>
                {issuedToken ? <Card className="issued-token"><strong>{t("settings.copyTokenNow")}</strong><code>{issuedToken.token}</code><div><Button size="sm" variant="secondary" onClick={() => { void navigator.clipboard.writeText(issuedToken.token).then(() => { setTokenCopied(true); createToken.reset(); }); }}><Copy size={14} />{tokenCopied ? t("settings.copied") : t("settings.copyToken")}</Button><Button size="sm" variant="ghost" onClick={() => { setIssuedToken(undefined); setTokenCopied(false); createToken.reset(); }}>{t("common.close")}</Button></div></Card> : null}
                {lanTokens.isLoading ? <LoadingState label={t("settings.loadingTokens")} /> : null}
                {lanTokens.isError ? <ErrorState error={lanTokens.error} onRetry={() => void lanTokens.refetch()} /> : null}
                {lanTokens.data?.length ? <div className="lan-token-list">{lanTokens.data.map((token) => <div key={token.id}><div><strong>{token.label}</strong><span>{token.lastUsedAt ? t("settings.tokenLastUsed", { date: new Date(token.lastUsedAt).toLocaleString(i18n.language) }) : t("settings.tokenNeverUsed")}</span></div><Button size="sm" variant="ghost" aria-label={t("settings.revokeToken", { name: token.label })} disabled={revokeToken.isPending} onClick={() => revokeToken.mutate(token.id)}><Trash2 size={15} /></Button></div>)}</div> : null}
                {createToken.isError ? <ErrorState error={createToken.error} onRetry={() => createToken.mutate()} /> : null}
                {revokeToken.isError ? <ErrorState error={revokeToken.error} /> : null}
              </div>
            </SettingsSection>
          </Tabs.Content>
          <Tabs.Content value="updates">
            <SettingsSection icon={<RefreshCw size={20} />} title={t("settings.updates")} description={t("settings.updateDeferred")}>
              <p>{t("settings.updateManual")}</p>
              {!desktop ? <p>{t("settings.updateDesktopOnly")}</p> : <div className="stack">
                <Button variant="secondary" disabled={checkUpdate.isPending || installUpdate.isPending} onClick={() => checkUpdate.mutate()}>
                  {checkUpdate.isPending ? <LoaderCircle className="spin" size={16} /> : <RefreshCw size={16} />}
                  {t("settings.checkNow")}
                </Button>
                {pendingUpdate === null ? <Card><strong>{t("settings.updateCurrent")}</strong></Card> : null}
                {pendingUpdate ? <Card className="stack">
                  <div>
                    <strong>{t("settings.updateAvailable", { version: pendingUpdate.version })}</strong>
                    <p>{pendingUpdate.body || t("settings.updateNoNotes")}</p>
                  </div>
                  {updateProgress ? <ProgressBar
                    value={updateProgress.total ? updateProgress.downloaded / updateProgress.total * 100 : 0}
                    label={t("settings.updateDownloading")}
                  /> : null}
                  <Button disabled={installUpdate.isPending} onClick={() => installUpdate.mutate()}>
                    {installUpdate.isPending ? <LoaderCircle className="spin" size={16} /> : <Download size={16} />}
                    {t("settings.approveUpdate")}
                  </Button>
                </Card> : null}
                {checkUpdate.isError ? <ErrorState error={checkUpdate.error} onRetry={() => checkUpdate.mutate()} /> : null}
                {installUpdate.isError ? <ErrorState error={installUpdate.error} onRetry={() => installUpdate.mutate()} /> : null}
              </div>}
            </SettingsSection>
          </Tabs.Content>
        </div>
      </Tabs.Root>
    </div>
  );
}

function parseHosts(value: string): string[] {
  if (!value.trim()) return [];
  return value.split(",").map((host) => host.trim()).filter(Boolean);
}

function editableSettingsPatch(value: AppSettings, persisted?: AppSettings): Partial<AppSettings> {
  const patch: Partial<AppSettings> = { ...value };
  delete patch.libraryPath;
  delete patch.cachePath;
  delete patch.secretStore;
  delete patch.firstRunComplete;
  if (persisted?.cacheLimitBytes === value.cacheLimitBytes) delete patch.cacheLimitBytes;
  return patch;
}

function SettingsSection({ icon, title, description, children }: { icon: React.ReactNode; title: string; description?: string; children: React.ReactNode }) {
  return <Card className="settings-section"><header><span>{icon}</span><div><h2>{title}</h2>{description ? <p>{description}</p> : null}</div></header><div className="settings-section-body">{children}</div></Card>;
}
