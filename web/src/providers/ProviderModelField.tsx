import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { AvailableProviderModel } from "../api/types";
import { Field, Input, Select } from "../components/ui";
import type { ProviderModelSource, ProviderRole } from "./presets";
import type { ProviderModelDiscoveryStatus } from "./useProviderModels";

interface ProviderModelFieldProps {
  role: ProviderRole;
  source: ProviderModelSource;
  value: string;
  models: AvailableProviderModel[];
  status: ProviderModelDiscoveryStatus;
  onChange: (model: string) => void;
}

export function ProviderModelField({
  role,
  source,
  value,
  models,
  status,
  onChange,
}: ProviderModelFieldProps) {
  const { t } = useTranslation();
  const [manual, setManual] = useState(false);
  const modelIds = models.map((model) => model.id);
  const discoveredValue = modelIds.includes(value);

  useEffect(() => {
    setManual(Boolean(value) && !modelIds.includes(value));
  }, [models, value]);

  if (source === "none") return null;
  const label = t(role === "tts" ? "providers.ttsModel" : "providers.llmModel");

  if (status === "loading") {
    return <Field label={label} hint={t("providers.loadingAvailableModels")}><Select disabled value=""><option>{t("providers.loadingAvailableModels")}</option></Select></Field>;
  }

  if (models.length > 0) {
    return <div className="provider-model-picker">
      <Field label={label} hint={t("providers.availableModelsHint")}>
        <Select
          value={manual ? "__manual__" : discoveredValue ? value : ""}
          onChange={(event) => {
            if (event.target.value === "__manual__") {
              setManual(true);
              return;
            }
            setManual(false);
            onChange(event.target.value);
          }}
        >
          <option value="">{t("providers.chooseModel")}</option>
          {models.map((model) => <option value={model.id} key={model.id}>{model.name === model.id ? model.id : `${model.name} (${model.id})`}</option>)}
          <option value="__manual__">{t("providers.customModel")}</option>
        </Select>
      </Field>
      {manual ? <Field label={t("providers.customModel")} hint={t("providers.customModelHint")}><Input value={value} onChange={(event) => onChange(event.target.value)} /></Field> : null}
    </div>;
  }

  const hint = status === "waiting"
    ? t("providers.modelDiscoveryWaiting")
    : status === "error"
      ? t("providers.modelDiscoveryFailed")
      : source === "default_only"
        ? t("providers.providerDefaultModelHint")
        : t("providers.noAvailableModels");
  return <Field label={label} hint={hint}><Input value={value} onChange={(event) => onChange(event.target.value)} /></Field>;
}
