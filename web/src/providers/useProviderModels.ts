import { useEffect, useState } from "react";
import { api } from "../api/client";
import type { AvailableProviderModel, ProviderKind, ProviderProfile, ProviderRole } from "../api/types";
import type { ProviderModelSource } from "./presets";

export type ProviderModelDiscoveryStatus = "idle" | "waiting" | "loading" | "success" | "error";

export interface ProviderModelDiscoveryState {
  status: ProviderModelDiscoveryStatus;
  models: AvailableProviderModel[];
  strict: boolean;
}

interface ProviderModelDiscoveryConfig {
  enabled: boolean;
  providerId?: string;
  credentialConfigured?: boolean;
  name: string;
  kind: ProviderKind;
  role: ProviderRole;
  mode: ProviderProfile["mode"];
  endpoint: string;
  executablePath?: string;
  workingDirectory?: string;
  argumentsText?: string;
  credential: string;
  modelSource: ProviderModelSource;
}

function argumentLines(value: string): string[] {
  return value.split(/\r?\n/).filter((argument) => argument.length > 0);
}

function hasKnownStrictCatalog(kind: ProviderKind, role: ProviderRole): boolean {
  return (kind === "openai" && (role === "tts" || role === "llm"))
    || (kind === "openai_tts" && role === "tts")
    || (kind === "piper" && role === "tts");
}

/**
 * Debounced model discovery that deliberately keeps credentials out of React Query keys/caches.
 * Only the final settled form value is sent to the local service.
 */
export function useProviderModels(config: ProviderModelDiscoveryConfig): ProviderModelDiscoveryState {
  const knownStrict = hasKnownStrictCatalog(config.kind, config.role);
  const [state, setState] = useState<ProviderModelDiscoveryState>({ status: "idle", models: [], strict: knownStrict });
  const managed = config.mode === "managed_child";
  const hasCredential = Boolean(config.credential) || Boolean(config.providerId && config.credentialConfigured);
  const ready = config.enabled
    && config.modelSource === "discover"
    && (config.mode === "native" || Boolean(config.endpoint.trim()))
    && (!managed || Boolean(config.executablePath?.trim()))
    && (config.mode !== "cloud_remote" || hasCredential);

  useEffect(() => {
    if (!config.enabled || config.modelSource === "none") {
      setState({ status: "idle", models: [], strict: knownStrict });
      return;
    }
    if (config.modelSource === "default_only") {
      setState({ status: "success", models: [], strict: knownStrict });
      return;
    }
    if (!ready) {
      setState({ status: "waiting", models: [], strict: knownStrict });
      return;
    }

    let cancelled = false;
    // Clear the previous role's catalog before the debounce. Otherwise a TTS list can remain
    // selectable for a short period after switching the same connection to LLM (or vice versa).
    setState({ status: "loading", models: [], strict: knownStrict });
    const timeout = window.setTimeout(() => {
      void api.discoverProviderModels({
        providerId: config.providerId,
        name: config.name.trim() || undefined,
        kind: config.kind,
        role: config.role,
        mode: config.mode,
        endpoint: config.endpoint.trim() || null,
        executablePath: managed ? config.executablePath?.trim() || null : null,
        workingDirectory: managed ? config.workingDirectory?.trim() || null : null,
        arguments: managed ? argumentLines(config.argumentsText ?? "") : [],
        credential: config.credential || undefined,
      }).then(
        (response) => {
          if (!cancelled) setState({ status: "success", models: response.items, strict: knownStrict || response.strict });
        },
        () => {
          if (!cancelled) setState({ status: "error", models: [], strict: knownStrict });
        },
      );
    }, 450);
    return () => {
      cancelled = true;
      window.clearTimeout(timeout);
    };
  }, [
    config.argumentsText,
    config.credential,
    config.credentialConfigured,
    config.enabled,
    config.endpoint,
    config.executablePath,
    config.kind,
    config.mode,
    config.modelSource,
    config.name,
    config.providerId,
    config.role,
    config.workingDirectory,
    knownStrict,
    managed,
    ready,
  ]);

  return state;
}
