// Builds the `fonsviva` daemon and puts it where Tauri bundles sidecars:
// src-tauri/binaries/fonsviva-engine-<target triple>[.exe]. The name differs from the app's own
// binary ("Fonsviva"), which would collide with "fonsviva" on case-insensitive file systems. `--release` builds the optimized binary.

import { copyFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";

const release = process.argv.includes("--release");
const root = join(import.meta.dir, "..", "..");
const triple =
  process.env.TAURI_ENV_TARGET_TRIPLE ??
  /host: (\S+)/.exec(Bun.spawnSync(["rustc", "-vV"]).stdout.toString())?.[1];
if (!triple) throw new Error("rustc -vV did not name a host triple");

const args = ["cargo", "build", "-p", "fonsviva", "--target", triple, "-j", process.env.CARGO_BUILD_JOBS ?? "3"];
if (release) args.push("--release");
const built = Bun.spawnSync(args, { cwd: root, stdout: "inherit", stderr: "inherit" });
if (built.exitCode !== 0) process.exit(built.exitCode ?? 1);

const exe = triple.includes("windows") ? ".exe" : "";
const from = join(root, "target", triple, release ? "release" : "debug", "fonsviva" + exe);
const dir = join(import.meta.dir, "..", "src-tauri", "binaries");
mkdirSync(dir, { recursive: true });
copyFileSync(from, join(dir, `fonsviva-engine-${triple}${exe}`));
console.log(`sidecar: fonsviva-engine-${triple}${exe}`);
