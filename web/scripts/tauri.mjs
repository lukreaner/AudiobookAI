import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDirectory = path.dirname(fileURLToPath(import.meta.url));
const desktopDirectory = path.resolve(scriptDirectory, "../../apps/desktop/src-tauri");
const executable = path.resolve(
  scriptDirectory,
  "../node_modules/.bin",
  process.platform === "win32" ? "tauri.cmd" : "tauri",
);
const [command = "dev", ...arguments_] = process.argv.slice(2);
const result = spawnSync(executable, [command, ...arguments_], {
  cwd: desktopDirectory,
  stdio: "inherit",
  shell: process.platform === "win32",
});

if (result.error) throw result.error;
process.exit(result.status ?? 1);
