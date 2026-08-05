import { useQuery } from "@tanstack/react-query";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { lazy, Suspense, useEffect } from "react";
import { Navigate, Outlet, Route, Routes, useLocation, useNavigate } from "react-router-dom";
import { ApiError, api } from "./api/client";
import { AppShell } from "./components/AppShell";
import { LoadingState } from "./components/StateViews";
import { useTranslation } from "react-i18next";

const ExportsPage = lazy(() => import("./pages/ExportsPage").then((module) => ({ default: module.ExportsPage })));
const DiagnosticsPage = lazy(() => import("./pages/DiagnosticsPage").then((module) => ({ default: module.DiagnosticsPage })));
const ImportPage = lazy(() => import("./pages/ImportPage").then((module) => ({ default: module.ImportPage })));
const JobsPage = lazy(() => import("./pages/JobsPage").then((module) => ({ default: module.JobsPage })));
const LibraryPage = lazy(() => import("./pages/LibraryPage").then((module) => ({ default: module.LibraryPage })));
const LoginPage = lazy(() => import("./pages/LoginPage").then((module) => ({ default: module.LoginPage })));
const ProjectPage = lazy(() => import("./pages/ProjectPage").then((module) => ({ default: module.ProjectPage })));
const ProvidersPage = lazy(() => import("./pages/ProvidersPage").then((module) => ({ default: module.ProvidersPage })));
const SettingsPage = lazy(() => import("./pages/SettingsPage").then((module) => ({ default: module.SettingsPage })));
const SetupPage = lazy(() => import("./pages/SetupPage").then((module) => ({ default: module.SetupPage })));
const UsagePage = lazy(() => import("./pages/UsagePage").then((module) => ({ default: module.UsagePage })));

export default function App() {
  const { t } = useTranslation();
  return (
    <Suspense fallback={<div className="startup-state"><LoadingState label={t("common.loading")} /></div>}>
      <Routes>
        <Route path="/login" element={<LoginPage />} />
        <Route path="/setup" element={<SetupPage />} />
        <Route element={<StartupGate />}>
          <Route element={<AppShell />}>
            <Route index element={<Navigate to="/library" replace />} />
            <Route path="/library" element={<LibraryPage />} />
            <Route path="/import" element={<ImportPage />} />
            <Route path="/projects/:id/chapters" element={<ProjectPage tab="chapters" />} />
            <Route path="/projects/:id/characters" element={<ProjectPage tab="characters" />} />
            <Route path="/projects/:id/pronunciation" element={<ProjectPage tab="pronunciation" />} />
            <Route path="/projects/:id/preflight" element={<ProjectPage tab="preflight" />} />
            <Route path="/providers" element={<ProvidersPage />} />
            <Route path="/jobs" element={<JobsPage />} />
            <Route path="/jobs/:id" element={<JobsPage />} />
            <Route path="/exports" element={<ExportsPage />} />
            <Route path="/usage" element={<UsagePage />} />
            <Route path="/diagnostics" element={<DiagnosticsPage />} />
            <Route path="/settings" element={<SettingsPage />} />
            <Route path="*" element={<Navigate to="/library" replace />} />
          </Route>
        </Route>
      </Routes>
    </Suspense>
  );
}

function StartupGate() {
  const { t, i18n } = useTranslation();
  const navigate = useNavigate();
  const location = useLocation();
  const settings = useQuery({
    queryKey: ["settings"],
    queryFn: api.settings,
    retry: (failureCount, error) => !(error instanceof ApiError && error.problem.status === 401) && failureCount < 2,
  });
  useEffect(() => {
    if (!settings.data) return;
    if (!settings.data.firstRunComplete) navigate("/setup", { replace: true });
    if (settings.data.language !== i18n.language) void i18n.changeLanguage(settings.data.language);
    if (settings.data.theme === "system") document.documentElement.removeAttribute("data-theme");
    else document.documentElement.dataset.theme = settings.data.theme;
    if (isTauri()) {
      void invoke("set_close_to_tray", { enabled: settings.data.closeToTray });
    }
  }, [i18n, location.pathname, navigate, settings.data]);
  if (settings.isLoading) return <div className="startup-state"><LoadingState label={t("state.loadingSettings")} /></div>;
  if (settings.error instanceof ApiError && settings.error.problem.status === 401) {
    return <Navigate to="/login" replace state={{ from: location.pathname }} />;
  }
  return <Outlet />;
}
