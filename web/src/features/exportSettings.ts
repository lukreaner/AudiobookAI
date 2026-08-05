import type { JobExportSettings } from "../api/types";

export interface ExportFormState extends Omit<JobExportSettings, "outputDirectory" | "fileName" | "backgroundMusicPath"> {
  outputDirectory: string;
  fileName: string;
  backgroundMusicPath: string;
}

export const DEFAULT_EXPORT_SETTINGS: ExportFormState = {
  format: "m4b",
  splitPerChapter: false,
  outputDirectory: "",
  fileName: "",
  bitrateKbps: 128,
  backgroundMusicPath: "",
  confirmBackgroundMusicOwned: false,
  musicGainDb: -24,
  ducking: true,
};

export function requiresMusicOwnership(settings: ExportFormState): boolean {
  return Boolean(settings.backgroundMusicPath.trim()) && !settings.confirmBackgroundMusicOwned;
}

export function toJobExportSettings(settings: ExportFormState): JobExportSettings {
  const outputDirectory = settings.outputDirectory.trim();
  const fileName = settings.fileName.trim();
  const backgroundMusicPath = settings.backgroundMusicPath.trim();

  return {
    format: settings.format,
    splitPerChapter: settings.splitPerChapter,
    ...(outputDirectory ? { outputDirectory } : {}),
    ...(fileName ? { fileName } : {}),
    bitrateKbps: settings.bitrateKbps,
    ...(backgroundMusicPath ? { backgroundMusicPath } : {}),
    confirmBackgroundMusicOwned: backgroundMusicPath ? settings.confirmBackgroundMusicOwned : false,
    musicGainDb: settings.musicGainDb,
    ducking: settings.ducking,
  };
}
