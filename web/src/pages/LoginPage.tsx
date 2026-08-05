import { useMutation, useQueryClient } from "@tanstack/react-query";
import { KeyRound, LoaderCircle, Waves } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useLocation, useNavigate } from "react-router-dom";
import { api } from "../api/client";
import { ErrorState } from "../components/StateViews";
import { Button, Card, Field, Input } from "../components/ui";

export function LoginPage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const location = useLocation();
  const queryClient = useQueryClient();
  const [password, setPassword] = useState("");
  const login = useMutation({
    mutationFn: () => api.loginLan(password),
    onSuccess: async () => {
      setPassword("");
      await queryClient.invalidateQueries({ queryKey: ["settings"] });
      const from = typeof location.state === "object" && location.state && "from" in location.state
        ? String(location.state.from)
        : "/library";
      navigate(from.startsWith("/") && from !== "/login" ? from : "/library", { replace: true });
    },
  });
  return (
    <main className="lan-login-shell">
      <Card className="lan-login-card">
        <div className="lan-login-icon"><Waves size={30} /></div>
        <h1>{t("login.title")}</h1>
        <p>{t("login.detail")}</p>
        <form onSubmit={(event) => { event.preventDefault(); if (password) login.mutate(); }}>
          <Field label={t("login.password")}><Input autoFocus type="password" autoComplete="current-password" value={password} onChange={(event) => setPassword(event.target.value)} /></Field>
          <Button type="submit" disabled={!password || login.isPending}>{login.isPending ? <LoaderCircle className="spin" size={16} /> : <KeyRound size={16} />}{t("login.signIn")}</Button>
        </form>
        {login.isError ? <ErrorState error={login.error} onRetry={() => login.mutate()} /> : null}
        <small>{t("login.ownerOnly")}</small>
      </Card>
    </main>
  );
}
