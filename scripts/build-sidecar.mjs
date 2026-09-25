// Build the `turbomerger` command line and stage it where Tauri's bundler
// looks for sidecars (`bundle.externalBin` in src-tauri/tauri.bundle.conf.json):
// src-tauri/binaries/turbomerger-<target-triple>[.exe].
//
// Runs as the bundle config's beforeBuildCommand, so `tauri build` exports
// TAURI_ENV_TARGET_TRIPLE and TAURI_ENV_DEBUG. By hand:
//   node scripts/build-sidecar.mjs [--target <triple>] [--debug]
import { execFileSync } from "node:child_process";
import { copyFileSync, chmodSync, mkdirSync } from "node:fs";
import { join } from "node:path";

const args = process.argv.slice(2);
const flag = (name) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : undefined;
};

const hostTriple = () =>
  execFileSync("rustc", ["--print", "host-tuple"], { encoding: "utf8" }).trim();

const host = hostTriple();
const target = flag("--target") || process.env.TAURI_ENV_TARGET_TRIPLE || host;
const debug = args.includes("--debug") || process.env.TAURI_ENV_DEBUG === "true";
const exe = target.includes("windows") ? ".exe" : "";
// A host build shares target/<profile> with the app; only cross builds get
// their own target/<triple>/ tree.
const cross = target !== host;

const cargoArgs = ["build", "--locked", "-p", "tm-cli", "--bin", "turbomerger"];
if (cross) cargoArgs.push("--target", target);
if (!debug) cargoArgs.push("--release");
console.log(`build-sidecar: cargo ${cargoArgs.join(" ")}`);
execFileSync("cargo", cargoArgs, { stdio: "inherit" });

const profileDir = cross ? join("target", target) : "target";
const built = join(profileDir, debug ? "debug" : "release", `turbomerger${exe}`);
const dir = join("src-tauri", "binaries");
mkdirSync(dir, { recursive: true });
const staged = join(dir, `turbomerger-${target}${exe}`);
copyFileSync(built, staged);
if (!exe) chmodSync(staged, 0o755);
console.log(`build-sidecar: ${built} -> ${staged}`);
