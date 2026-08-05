import * as Tabs from "@radix-ui/react-tabs";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertCircle, Calculator, Gauge, Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "../api/client";
import type { Budget, RateCard } from "../api/types";
import { EmptyState, ErrorState, LoadingState } from "../components/StateViews";
import { Badge, Button, Card, Dialog, Field, Input, PageHeading, ProgressBar, Select, Stat, SwitchField } from "../components/ui";
import { formatCount, formatDate, formatMoney } from "../lib/format";

function budgetValue(value: number, budget: Budget, locale: string): string {
  if (budget.metric === "money") return formatMoney(value, budget.currency, locale);
  return formatCount(value, locale);
}

function pricingLabel(key: string, t: (key: string) => string): string {
  const labels: Record<string, string> = {
    per_character_micros: t("usage.pricePerCharacter"),
    per_1000_characters_micros: t("usage.pricePerThousandCharacters"),
    per_input_token_micros: t("usage.pricePerInputToken"),
    per_output_token_micros: t("usage.pricePerOutputToken"),
    per_cached_input_token_micros: t("usage.pricePerCachedToken"),
    per_reasoning_token_micros: t("usage.pricePerReasoningToken"),
    per_1m_input_tokens_micros: t("usage.pricePerMillionInput"),
    per_1m_output_tokens_micros: t("usage.pricePerMillionOutput"),
    per_1m_cached_input_tokens_micros: t("usage.pricePerMillionCached"),
    per_1m_reasoning_tokens_micros: t("usage.pricePerMillionReasoning"),
  };
  return labels[key] ?? key;
}

export function UsagePage() {
  const { t, i18n } = useTranslation();
  const queryClient = useQueryClient();
  const usage = useQuery({ queryKey: ["usage"], queryFn: api.usage });
  const budgets = useQuery({ queryKey: ["budgets"], queryFn: api.budgets });
  const rateCards = useQuery({ queryKey: ["rate-cards"], queryFn: api.rateCards });
  const providers = useQuery({ queryKey: ["providers"], queryFn: api.providers });
  const [open, setOpen] = useState(false);
  const [rateOpen, setRateOpen] = useState(false);
  const [form, setForm] = useState({ name: "", providerProfileId: "", period: "monthly" as Budget["period"], metric: "money" as Budget["metric"], limit: "", currency: "EUR", hard: true, warningPercent: "80" });
  const [rateForm, setRateForm] = useState({ providerProfileId: "", workload: "tts" as RateCard["workload"], model: "", currency: "EUR", source: "User configured", sourceUrl: "", ttsPerThousand: "", inputPerMillion: "", outputPerMillion: "" });
  const create = useMutation({
    mutationFn: () => api.createBudget({
      name: form.name,
      providerProfileId: form.providerProfileId || undefined,
      period: form.period,
      metric: form.metric,
      limit: form.metric === "money" ? Math.round(Number(form.limit) * 1_000_000) : Number(form.limit),
      currency: form.metric === "money" ? form.currency : undefined,
      hard: form.hard,
      warningPercent: Number(form.warningPercent),
    }),
    onSuccess: async () => { await queryClient.invalidateQueries({ queryKey: ["budgets"] }); setOpen(false); },
  });
  const removeBudget = useMutation({ mutationFn: api.deleteBudget, onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["budgets"] }) });
  const createRate = useMutation({
    mutationFn: () => api.createRateCard({
      providerProfileId: rateForm.providerProfileId,
      model: rateForm.model.trim() || undefined,
      workload: rateForm.workload,
      currency: rateForm.currency,
      source: rateForm.source,
      sourceUrl: rateForm.sourceUrl.trim() || undefined,
      pricing: rateForm.workload === "tts"
        ? { per_1000_characters_micros: Math.round(Number(rateForm.ttsPerThousand) * 1_000_000) }
        : {
            per_1m_input_tokens_micros: Math.round(Number(rateForm.inputPerMillion) * 1_000_000),
            per_1m_output_tokens_micros: Math.round(Number(rateForm.outputPerMillion) * 1_000_000),
          },
    }),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["rate-cards"] });
      setRateOpen(false);
      setRateForm({ providerProfileId: "", workload: "tts", model: "", currency: "EUR", source: "User configured", sourceUrl: "", ttsPerThousand: "", inputPerMillion: "", outputPerMillion: "" });
    },
  });
  const removeRate = useMutation({ mutationFn: api.deleteRateCard, onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["rate-cards"] }) });
  if (usage.isLoading || budgets.isLoading || rateCards.isLoading) return <LoadingState label={t("state.loadingUsage")} />;
  if (usage.isError) return <ErrorState error={usage.error} onRetry={() => void usage.refetch()} />;
  if (budgets.isError) return <ErrorState error={budgets.error} onRetry={() => void budgets.refetch()} />;
  if (rateCards.isError) return <ErrorState error={rateCards.error} onRetry={() => void rateCards.refetch()} />;
  const summary = usage.data;
  const ratePriceValid = rateForm.workload === "tts"
    ? rateForm.ttsPerThousand.trim() !== "" && Number.isFinite(Number(rateForm.ttsPerThousand)) && Number(rateForm.ttsPerThousand) >= 0
    : [rateForm.inputPerMillion, rateForm.outputPerMillion].every((value) => value.trim() !== "" && Number.isFinite(Number(value)) && Number(value) >= 0);
  return (
    <div className="page usage-page">
      <PageHeading
        eyebrow={t("usage.eyebrow")}
        title={t("usage.title")}
        subtitle={t("usage.subtitle")}
        actions={<><Button variant="secondary" onClick={() => setRateOpen(true)}><Calculator size={16} />{t("usage.addRateCard")}</Button><Button onClick={() => setOpen(true)}><Plus size={16} />{t("usage.addBudget")}</Button></>}
      />
      <div className="usage-stats grid-4">
        <Card><Stat label={t("usage.spend")} value={formatMoney(summary?.monetaryCostMicros, summary?.currency, i18n.language)} detail={summary?.unknownCostRequests ? t("usage.unknownRequests", { count: summary.unknownCostRequests }) : t("usage.thisPeriod")} /></Card>
        <Card><Stat label={t("usage.characters")} value={formatCount(summary?.characters, i18n.language)} detail={t("usage.thisPeriod")} /></Card>
        <Card><Stat label={t("usage.inputTokens")} value={formatCount(summary?.inputTokens, i18n.language)} detail={t("usage.thisPeriod")} /></Card>
        <Card><Stat label={t("usage.outputTokens")} value={formatCount(summary?.outputTokens, i18n.language)} detail={t("usage.thisPeriod")} /></Card>
      </div>
      {summary?.unknownCostRequests ? <Card className="unknown-usage"><AlertCircle size={19} /><p>{t("usage.unknownRequests", { count: summary.unknownCostRequests })}</p></Card> : null}
      <Tabs.Root className="usage-tabs" defaultValue="budgets">
        <Tabs.List className="tab-list" aria-label={t("usage.title")}><Tabs.Trigger value="budgets">{t("usage.budgets")}</Tabs.Trigger><Tabs.Trigger value="rate-cards">{t("usage.rateCards")}</Tabs.Trigger><Tabs.Trigger value="ledger">{t("usage.ledger")}</Tabs.Trigger></Tabs.List>
        <Tabs.Content value="budgets">
          {!budgets.data?.items.length ? <EmptyState title={t("usage.emptyBudgets")} detail={t("usage.subtitle")} action={<Button onClick={() => setOpen(true)}><Plus size={16} />{t("usage.addBudget")}</Button>} /> : <div className="budget-grid">{budgets.data.items.map((budget) => { const total = budget.used + budget.reserved; const percentage = budget.limit > 0 ? total / budget.limit * 100 : 0; return <Card className="budget-card" key={budget.id}><div className="space-between"><span className="budget-icon"><Gauge size={18} /></span><span className="card-actions"><Badge tone={budget.hard ? "danger" : "warning"}>{budget.hard ? t("usage.hard") : t("usage.warning")}</Badge><Button size="sm" variant="ghost" aria-label={t("usage.deleteBudget", { name: budget.name })} title={t("usage.deleteBudget", { name: budget.name })} disabled={removeBudget.isPending} onClick={() => { if (window.confirm(t("usage.deleteBudgetConfirm", { name: budget.name }))) removeBudget.mutate(budget.id); }}><Trash2 size={15} /></Button></span></div><h2>{budget.name}</h2><p>{budget.providerProfileId ? providers.data?.items.find((provider) => provider.id === budget.providerProfileId)?.name || t("common.unknown") : t("usage.global")} · {t(`usage.${budget.period}`)}</p><div className="budget-numbers"><strong>{budgetValue(total, budget, i18n.language)}</strong><span>{t("usage.limit", { value: budgetValue(budget.limit, budget, i18n.language) })}</span></div><ProgressBar value={percentage} label={t("usage.used", { used: budgetValue(budget.used, budget, i18n.language) })} tone={percentage >= 100 ? "warning" : "accent"} /><div className="budget-detail"><span>{t("usage.used", { used: budgetValue(budget.used, budget, i18n.language) })}</span><span>{t("usage.reserved", { reserved: budgetValue(budget.reserved, budget, i18n.language) })}</span></div></Card>; })}</div>}
        </Tabs.Content>
        <Tabs.Content value="rate-cards">
          {!rateCards.data?.items.length ? <EmptyState title={t("usage.emptyRateCards")} detail={t("usage.rateCardsDetail")} action={<Button onClick={() => setRateOpen(true)}><Calculator size={16} />{t("usage.addRateCard")}</Button>} /> : <div className="budget-grid">{rateCards.data.items.map((rate) => { const provider = providers.data?.items.find((item) => item.id === rate.providerProfileId); return <Card className="budget-card rate-card" key={rate.id}><div className="space-between"><span className="budget-icon"><Calculator size={18} /></span><span className="card-actions"><Badge tone="neutral">{t(`usage.${rate.workload}`)}</Badge><Button size="sm" variant="ghost" aria-label={t("usage.deleteRateCard")} title={t("usage.deleteRateCard")} disabled={removeRate.isPending} onClick={() => { if (window.confirm(t("usage.deleteRateCardConfirm"))) removeRate.mutate(rate.id); }}><Trash2 size={15} /></Button></span></div><h2>{provider?.name ?? t("common.unknown")}</h2><p>{rate.model || t("usage.allModels")} · {rate.currency}</p><dl className="rate-pricing">{Object.entries(rate.pricing).map(([key, value]) => <div key={key}><dt>{pricingLabel(key, t)}</dt><dd>{formatMoney(value, rate.currency, i18n.language)}</dd></div>)}</dl><div className="budget-detail"><span>{rate.source}</span><span>{t("usage.effective", { date: formatDate(rate.effectiveAt, i18n.language) })}</span></div></Card>; })}</div>}
        </Tabs.Content>
        <Tabs.Content value="ledger">
          {!summary?.rows.length ? <EmptyState title={t("usage.emptyLedger")} detail={t("usage.subtitle")} /> : <div className="table-wrap"><table className="data-table"><thead><tr><th>{t("import.titleLabel")}</th><th>{t("providers.title")}</th><th>{t("common.status")}</th><th>{t("usage.characters")}</th><th>{t("usage.inputTokens")}</th><th>{t("usage.spend")}</th></tr></thead><tbody>{summary.rows.map((row) => <tr key={row.id}><td><strong>{row.projectTitle || t("common.unknown")}</strong><small>{formatDate(row.occurredAt, i18n.language)}</small></td><td>{row.providerName}<small>{row.model || row.voice}</small></td><td><Badge tone={row.provenance === "reported" ? "positive" : row.provenance === "estimated" ? "warning" : "neutral"}>{t(`usage.${row.provenance}`)}</Badge></td><td>{formatCount(row.characters, i18n.language)}</td><td>{formatCount(row.inputTokens, i18n.language)}</td><td>{row.costMicros == null ? t("usage.unknownValue") : formatMoney(row.costMicros, row.currency, i18n.language)}</td></tr>)}</tbody></table></div>}
        </Tabs.Content>
      </Tabs.Root>

      <Dialog open={open} onOpenChange={setOpen} title={t("usage.addBudget")} description={t("usage.subtitle")} footer={<><Button variant="secondary" onClick={() => setOpen(false)}>{t("common.cancel")}</Button><Button disabled={!form.name || !Number(form.limit) || create.isPending} onClick={() => create.mutate()}>{t("common.add")}</Button></>}>
        <div className="stack">
          <Field label={t("usage.budgetName")}><Input value={form.name} onChange={(event) => setForm({ ...form, name: event.target.value })} /></Field>
          <Field label={t("usage.providerScope")}><Select value={form.providerProfileId} onChange={(event) => setForm({ ...form, providerProfileId: event.target.value })}><option value="">{t("usage.global")}</option>{providers.data?.items.map((provider) => <option key={provider.id} value={provider.id}>{provider.name}</option>)}</Select></Field>
          <div className="grid-2"><Field label={t("usage.period")}><Select value={form.period} onChange={(event) => setForm({ ...form, period: event.target.value as Budget["period"] })}>{(["job", "daily", "monthly", "lifetime"] as const).map((value) => <option key={value} value={value}>{t(`usage.${value}`)}</option>)}</Select></Field><Field label={t("usage.metric")}><Select value={form.metric} onChange={(event) => setForm({ ...form, metric: event.target.value as Budget["metric"] })}><option value="money">{t("usage.spend")}</option><option value="tokens">{t("usage.tokens")}</option><option value="characters">{t("usage.characters")}</option><option value="credits">{t("usage.credits")}</option></Select></Field></div>
          <div className="grid-2"><Field label={t("usage.amount")}><Input type="number" min="0" step={form.metric === "money" ? "0.01" : "1"} value={form.limit} onChange={(event) => setForm({ ...form, limit: event.target.value })} /></Field>{form.metric === "money" ? <Field label={t("usage.currency")}><Input maxLength={3} value={form.currency} onChange={(event) => setForm({ ...form, currency: event.target.value.toUpperCase() })} /></Field> : <Field label={t("usage.warningThreshold")}><Input type="number" min="1" max="100" value={form.warningPercent} onChange={(event) => setForm({ ...form, warningPercent: event.target.value })} /></Field>}</div>
          <SwitchField checked={form.hard} onCheckedChange={(hard) => setForm({ ...form, hard })} label={t("usage.hard")} detail={t("usage.hardDetail")} />
          {create.isError ? <ErrorState error={create.error} /> : null}
        </div>
      </Dialog>
      <Dialog open={rateOpen} onOpenChange={setRateOpen} title={t("usage.addRateCard")} description={t("usage.rateCardsDetail")} footer={<><Button variant="secondary" onClick={() => setRateOpen(false)}>{t("common.cancel")}</Button><Button disabled={!rateForm.providerProfileId || !rateForm.source.trim() || rateForm.currency.trim().length !== 3 || !ratePriceValid || createRate.isPending} onClick={() => createRate.mutate()}>{t("common.add")}</Button></>}>
        <div className="stack">
          <div className="grid-2"><Field label={t("usage.providerScope")}><Select value={rateForm.providerProfileId} onChange={(event) => setRateForm({ ...rateForm, providerProfileId: event.target.value })}><option value="">{t("common.select")}</option>{providers.data?.items.map((provider) => <option key={provider.id} value={provider.id}>{provider.name}</option>)}</Select></Field><Field label={t("usage.workload")}><Select value={rateForm.workload} onChange={(event) => setRateForm({ ...rateForm, workload: event.target.value as RateCard["workload"] })}><option value="tts">{t("usage.tts")}</option><option value="character_detection">{t("usage.character_detection")}</option></Select></Field></div>
          <div className="grid-2"><Field label={t("providers.model")} hint={t("usage.allModelsHint")}><Input value={rateForm.model} onChange={(event) => setRateForm({ ...rateForm, model: event.target.value })} /></Field><Field label={t("usage.currency")}><Input maxLength={3} value={rateForm.currency} onChange={(event) => setRateForm({ ...rateForm, currency: event.target.value.toUpperCase() })} /></Field></div>
          {rateForm.workload === "tts" ? <Field label={t("usage.pricePerThousandCharacters")} hint={t("usage.priceUnitHint")}><Input type="number" min="0" step="0.000001" value={rateForm.ttsPerThousand} onChange={(event) => setRateForm({ ...rateForm, ttsPerThousand: event.target.value })} /></Field> : <div className="grid-2"><Field label={t("usage.pricePerMillionInput")} hint={t("usage.priceUnitHint")}><Input type="number" min="0" step="0.000001" value={rateForm.inputPerMillion} onChange={(event) => setRateForm({ ...rateForm, inputPerMillion: event.target.value })} /></Field><Field label={t("usage.pricePerMillionOutput")} hint={t("usage.priceUnitHint")}><Input type="number" min="0" step="0.000001" value={rateForm.outputPerMillion} onChange={(event) => setRateForm({ ...rateForm, outputPerMillion: event.target.value })} /></Field></div>}
          <Field label={t("usage.rateSource")}><Input value={rateForm.source} onChange={(event) => setRateForm({ ...rateForm, source: event.target.value })} /></Field>
          <Field label={t("usage.rateSourceUrl")} hint={t("common.optional")}><Input type="url" value={rateForm.sourceUrl} onChange={(event) => setRateForm({ ...rateForm, sourceUrl: event.target.value })} /></Field>
          {createRate.isError ? <ErrorState error={createRate.error} /> : null}
        </div>
      </Dialog>
    </div>
  );
}
