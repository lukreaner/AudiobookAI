import { useQuery } from "@tanstack/react-query";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { clsx } from "clsx";
import {
  AudioLines,
  BookOpen,
  Boxes,
  ChevronLeft,
  ChevronRight,
  CircleDollarSign,
  Download,
  FileSearch,
  Menu,
  Plus,
  Power,
  Settings,
  SlidersHorizontal,
  X,
} from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { NavLink, Outlet, useLocation, useNavigate } from "react-router-dom";
import { api } from "../api/client";
import { Badge, Button, Dialog, IconButton } from "./ui";

const navigation = [
  { to: "/library", label: "nav.library", icon: BookOpen },
  { to: "/providers", label: "nav.providers", icon: SlidersHorizontal },
  { to: "/jobs", label: "nav.jobs", icon: Boxes },
  { to: "/exports", label: "nav.exports", icon: Download },
  { to: "/usage", label: "nav.usage", icon: CircleDollarSign },
] as const;

export function AppShell() {
  const { t } = useTranslation();
  const location = useLocation();
  const navigate = useNavigate();
  const [collapsed, setCollapsed] = useState(false);
  const [mobileOpen, setMobileOpen] = useState(false);
  const [quitOpen, setQuitOpen] = useState(false);
  const [quitting, setQuitting] = useState(false);
  const [quitError, setQuitError] = useState(false);
  const desktop = isTauri();
  const health = useQuery({ queryKey: ["health"], queryFn: api.health, retry: 2, refetchInterval: 15_000 });
  const status = health.isError ? "offline" : health.data?.status ?? "starting";
  const statusLabel = status === "ready"
    ? t("shell.serviceReady")
    : status === "degraded"
      ? t("shell.serviceDegraded")
      : status === "offline"
        ? t("shell.serviceOffline")
        : t("shell.serviceStarting");

  useEffect(() => {
    let unlistenImport: (() => void) | undefined;
    let unlistenEpub: (() => void) | undefined;
    void import("@tauri-apps/api/event").then(async ({ listen }) => {
      unlistenImport = await listen("audiobookai://show-import", () => navigate("/import"));
      unlistenEpub = await listen<string>("audiobookai://open-epub", (event) => navigate("/import", { state: { sourcePath: event.payload } }));
    }).catch(() => undefined);
    const initialEpub = window.__AUDIOBOOKAI_OPEN_EPUB__;
    delete window.__AUDIOBOOKAI_OPEN_EPUB__;
    if (initialEpub) navigate("/import", { state: { sourcePath: initialEpub } });
    return () => { unlistenImport?.(); unlistenEpub?.(); };
  }, [navigate]);

  const quitApplication = async () => {
    setQuitting(true);
    setQuitError(false);
    try {
      await invoke("quit_application");
    } catch {
      setQuitting(false);
      setQuitError(true);
    }
  };

  const nav = (
    <>
      <div className="brand-row">
        <NavLink to="/library" className="brand" aria-label="AudiobookAI">
          <span className="brand-mark" aria-hidden="true"><i /><i /><i /></span>
          {!collapsed ? <span>Audiobook<span>AI</span></span> : null}
        </NavLink>
        <IconButton className="mobile-close" label={t("common.close")} onClick={() => setMobileOpen(false)}><X size={19} /></IconButton>
      </div>

      <nav className="sidebar-nav" aria-label={t("nav.work")}>
        {!collapsed ? <p className="nav-label">{t("nav.work")}</p> : null}
        {navigation.map((item) => {
          const Icon = item.icon;
          return (
            <NavLink
              key={item.to}
              to={item.to}
              className={({ isActive }) => clsx("nav-item", isActive && "active")}
              onClick={() => setMobileOpen(false)}
              title={collapsed ? t(item.label) : undefined}
            >
              <Icon size={19} strokeWidth={1.8} />
              {!collapsed ? <span>{t(item.label)}</span> : null}
            </NavLink>
          );
        })}
      </nav>

      <div className="sidebar-bottom">
        {!collapsed ? <p className="nav-label">{t("nav.system")}</p> : null}
        <NavLink
          to="/diagnostics"
          className={({ isActive }) => clsx("nav-item", isActive && "active")}
          onClick={() => setMobileOpen(false)}
          title={collapsed ? t("nav.diagnostics") : undefined}
        >
          <FileSearch size={19} strokeWidth={1.8} />
          {!collapsed ? <span>{t("nav.diagnostics")}</span> : null}
        </NavLink>
        <NavLink
          to="/settings"
          className={({ isActive }) => clsx("nav-item", isActive && "active")}
          onClick={() => setMobileOpen(false)}
          title={collapsed ? t("nav.settings") : undefined}
        >
          <Settings size={19} strokeWidth={1.8} />
          {!collapsed ? <span>{t("nav.settings")}</span> : null}
        </NavLink>
        <div className={clsx("service-pill", `service-${status}`)} title={health.error instanceof Error ? health.error.message : undefined}>
          <span className="service-dot" />
          {!collapsed ? <span>{statusLabel}</span> : null}
        </div>
        <button className="collapse-button" type="button" onClick={() => setCollapsed((value) => !value)}>
          {collapsed ? <ChevronRight size={17} /> : <ChevronLeft size={17} />}
          {!collapsed ? t("nav.collapse") : <span className="sr-only">{t("nav.expand")}</span>}
        </button>
      </div>
    </>
  );

  return (
    <div className={clsx("app-shell", collapsed && "sidebar-collapsed")}>
      <a className="skip-link" href="#main-content">{t("shell.skipContent")}</a>
      <aside className="sidebar desktop-sidebar">{nav}</aside>
      {mobileOpen ? <button className="mobile-overlay" aria-label={t("common.close")} onClick={() => setMobileOpen(false)} /> : null}
      <aside className={clsx("sidebar mobile-sidebar", mobileOpen && "open")}>{nav}</aside>
      <div className="app-column">
        <header className="topbar">
          <IconButton className="mobile-menu" label={t("common.menu")} onClick={() => setMobileOpen(true)}><Menu size={20} /></IconButton>
          <div className="topbar-context">
            <AudioLines size={18} />
            <span>{t("shell.localPrivate")}</span>
          </div>
          <div className="topbar-actions">
            {location.pathname !== "/import" ? (
              <Button size="sm" onClick={() => navigate("/import")}><Plus size={16} />{t("shell.quickImport")}</Button>
            ) : null}
            <Badge tone={status === "ready" ? "positive" : status === "degraded" ? "warning" : "neutral"}>
              <span className="service-dot" />{statusLabel}
            </Badge>
            {desktop ? <Button size="sm" variant="ghost" onClick={() => setQuitOpen(true)}><Power size={16} />{t("shell.quit")}</Button> : null}
          </div>
        </header>
        <main id="main-content" className="main-content" tabIndex={-1}>
          <Outlet />
        </main>
      </div>
      {desktop ? <Dialog
        open={quitOpen}
        onOpenChange={(open) => { if (!quitting) { setQuitOpen(open); setQuitError(false); } }}
        title={t("shell.quitTitle")}
        description={t("shell.quitDetail")}
        size="sm"
        footer={<>
          <Button variant="secondary" disabled={quitting} onClick={() => setQuitOpen(false)}>{t("common.cancel")}</Button>
          <Button variant="danger" disabled={quitting} onClick={() => void quitApplication()}><Power size={16} />{quitting ? t("shell.quitting") : t("shell.quitConfirm")}</Button>
        </>}
      >
        <p className="quit-safety-note">{t("shell.quitSafety")}</p>
        {quitError ? <p className="provider-form-warning" role="alert">{t("shell.quitFailed")}</p> : null}
      </Dialog> : null}
    </div>
  );
}
