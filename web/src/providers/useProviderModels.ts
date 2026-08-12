import { useEffect, useState } from "react";
import { api } from "../api/client";
import type { AvailableProviderModel, ProviderKind, ProviderProfile } from "../api/types";
import type { ProviderModelSource } from "./presets";

export type ProviderModelDiscoveryStatus = "idle" | "waiting" | "loading" | "success" | "error";

export interface ProviderModelDiscoveryState {
  status: ProviderModelDiscoveryStatus;
  models: AvailableProviderModel[];
}

interface ProviderModelDiscoveryConfig {
  enabled: boolean;
  providerId?: string;
  credentialConfigured?: boolean;
  name: string;
  kind: ProviderKind;
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

/**
 * Debounced model discovery that deliberately keeps credentials out of React Query keys/caches.
 * Only the final settled form value is sent to the local service.
 */
export function useProviderModels(config: ProviderModelDiscoveryConfig): ProviderModelDiscoveryState {
  const [state, setState] = useState<ProviderModelDiscoveryState>({ status: "idle", models: [] });
  const managed = config.mode === "managed_child";
  const hasCredential = Boolean(config.credential) || Boolean(config.providerId && config.credentialConfigured);
  const ready = config.enabled
    && config.modelSource === "discover"
    && config.mode !== "native"
    && Boolean(config.endpoint.trim())
    && (!managed || Boolean(config.executablePath?.trim()))
    && (config.mode !== "cloud_remote" || hasCredential);

  useEffect(() => {
    if (!config.enabled || config.modelSource === "none") {
      setState({ status: "idle", models: [] });
      return;
    }
    if (config.modelSource === "default_only") {
      setState({ status: "success", models: [] });
      return;
    }
    if (!ready) {
      setState({ status: "waiting", models: [] });
      return;
    }

    let cancelled = false;
    const timeout = window.setTimeout(() => {
      setState({ status: "loading", models: [] });
      void api.discoverProviderModels({
        providerId: config.providerId,
        name: config.name.trim() || undefined,
        kind: config.kind,
        mode: config.mode,
        endpoint: config.endpoint.trim() || null,
        executablePath: managed ? config.executablePath?.trim() || null : null,
        workingDirectory: managed ? config.workingDirectory?.trim() || null : null,
        arguments: managed ? argumentLines(config.argumentsText ?? "") : [],
        credential: config.credential || undefined,
      }).then(
        (response) => {
          if (!cancelled) setState({ status: "success", models: response.items });
        },
        () => {
          if (!cancelled) setState({ status: "error", models: [] });
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
    config.workingDirectory,
    managed,
    ready,
  ]);

  return state;
}
