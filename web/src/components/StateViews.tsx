import { AlertCircle, BookOpen, CircleOff, LoaderCircle, RotateCw } from "lucide-react";
import { useTranslation } from "react-i18next";
import { ApiError } from "../api/client";
import { Button, Card } from "./ui";

export function LoadingState({ label }: { label: string }) {
  return (
    <div className="state-view" role="status" aria-live="polite">
      <LoaderCircle className="spin" size={24} />
      <p>{label}</p>
    </div>
  );
}

export function EmptyState({
  title,
  detail,
  action,
  icon = "book",
}: {
  title: string;
  detail: string;
  action?: React.ReactNode;
  icon?: "book" | "offline";
}) {
  const Icon = icon === "offline" ? CircleOff : BookOpen;
  return (
    <Card className="empty-state">
      <div className="empty-icon"><Icon size={23} /></div>
      <h2>{title}</h2>
      <p>{detail}</p>
      {action ? <div className="empty-action">{action}</div> : null}
    </Card>
  );
}

export function ErrorState({ error, onRetry }: { error: unknown; onRetry?: () => void }) {
  const { t } = useTranslation();
  const apiError = error instanceof ApiError ? error : undefined;
  const offline = apiError?.problem.status === 0;
  const forbidden = apiError?.problem.status === 401 || apiError?.problem.status === 403;
  const notFound = apiError?.problem.status === 404;
  const title = offline
    ? t("errors.offlineTitle")
    : forbidden
      ? t("errors.forbiddenTitle")
      : notFound
        ? t("errors.notFoundTitle")
        : t("errors.title");
  const detail = offline
    ? t("errors.offlineDetail")
    : forbidden
      ? t("errors.forbiddenDetail")
      : notFound
        ? t("errors.notFoundDetail")
        : apiError?.problem.detail || (error instanceof Error ? error.message : t("common.unknown"));
  return (
    <Card className="error-state" role="alert">
      <div className="error-icon"><AlertCircle size={22} /></div>
      <div>
        <h2>{title}</h2>
        <p>{detail}</p>
        {apiError?.problem.code ? <code>{t("errors.technical", { code: apiError.problem.code })}</code> : null}
      </div>
      {onRetry ? <Button variant="secondary" onClick={onRetry}><RotateCw size={16} />{t("errors.retry")}</Button> : null}
    </Card>
  );
}
