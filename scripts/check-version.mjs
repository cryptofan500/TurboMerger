import { readFileSync } from "node:fs";

const readJson = (path) => JSON.parse(readFileSync(path, "utf8"));
const packageVersion = readJson("package.json").version;
const tauriVersion = readJson("src-tauri/tauri.conf.json").version;

// The Rust version lives once, in the workspace root; every member inherits it.
const workspaceManifest = readFileSync("Cargo.toml", "utf8");
const cargoVersion = workspaceManifest.match(
  /^\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"/m,
)?.[1];
if (!cargoVersion) {
  throw new Error("Could not read [workspace.package].version from Cargo.toml");
}
const members = workspaceManifest
  .match(/^members\s*=\s*\[([^\]]*)\]/m)?.[1]
  .match(/"([^"]+)"/g)
  ?.map((m) => m.slice(1, -1));
if (!members?.length) {
  throw new Error("Could not read [workspace].members from Cargo.toml");
}
for (const member of members) {
  const manifest = readFileSync(`${member}/Cargo.toml`, "utf8");
  if (!/^version\.workspace\s*=\s*true/m.test(manifest)) {
    throw new Error(`${member}/Cargo.toml must use version.workspace = true`);
  }
}

const versions = new Map([
  ["package.json", packageVersion],
  ["Cargo.toml [workspace.package]", cargoVersion],
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
