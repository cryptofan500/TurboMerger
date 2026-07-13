import { readFileSync } from "node:fs";

const readJson = (path) => JSON.parse(readFileSync(path, "utf8"));
const packageVersion = readJson("package.json").version;
const tauriVersion = readJson("src-tauri/tauri.conf.json").version;
const cargoManifest = readFileSync("src-tauri/Cargo.toml", "utf8");
const cargoVersion = cargoManifest.match(
  /^\[package\][\s\S]*?^version\s*=\s*"([^"]+)"/m,
)?.[1];

if (!cargoVersion) {
  throw new Error("Could not read [package].version from src-tauri/Cargo.toml");
}

const versions = new Map([
  ["package.json", packageVersion],
  ["src-tauri/Cargo.toml", cargoVersion],
  ["src-tauri/tauri.conf.json", tauriVersion],
]);
const unique = new Set(versions.values());

if (unique.size !== 1) {
  throw new Error(
    `Version mismatch: ${[...versions].map(([file, version]) => `${file}=${version}`).join(", ")}`,
  );
}

const requestedTag = process.argv[2];
if (requestedTag) {
  const expected = requestedTag.replace(/^v/, "");
  if (!/^\d+\.\d+\.\d+$/.test(expected)) {
    throw new Error(`Release tag must be vMAJOR.MINOR.PATCH; got ${requestedTag}`);
  }
  if (expected !== packageVersion) {
    throw new Error(`Release tag ${requestedTag} does not match version ${packageVersion}`);
  }
}

console.log(`TurboMerger version ${packageVersion} is consistent.`);
