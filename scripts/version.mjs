// tuneKore's version, kept the same everywhere it is written down.
//
//   node scripts/version.mjs           prints the version; fails if the copies disagree (CI runs this)
//   node scripts/version.mjs 0.1.2     sets every copy to 0.1.2
//   node scripts/version.mjs patch     bumps the patch number (also: minor, major)
//
// Every pull request that adds to the app bumps the patch number and adds
// its entry to CHANGELOG.md. The stem worker (stems/) is versioned on its own.
import { readFileSync, writeFileSync } from "node:fs";

const SEMVER = /^\d+\.\d+\.\d+$/;

// Each place the app's version lives, with how to read and write it.
const json = (file, ...paths) => ({
  file,
  read: (text) => paths.map((p) => p.reduce((o, k) => o?.[k], JSON.parse(text))),
  write: (text, v) => {
    const data = JSON.parse(text);
    for (const p of paths) p.slice(0, -1).reduce((o, k) => o[k], data)[p.at(-1)] = v;
    return JSON.stringify(data, null, 2) + "\n";
  },
});
const cargoPackage = (file) => ({
  file,
  read: (text) => [text.match(/^\[package\][^[]*?^version = "([^"]+)"/ms)?.[1]],
  write: (text, v) => text.replace(/^(\[package\][^[]*?^version = ")[^"]+(")/ms, `$1${v}$2`),
});
const OURS = ["tunekore", "tunekore-engine", "tunekore-analyzer"];
const cargoLock = {
  file: "Cargo.lock",
  // \r?\n: Windows checkouts have CRLF line endings.
  read: (text) => OURS.map((n) => text.match(new RegExp(`name = "${n}"\\r?\\nversion = "([^"]+)"`))?.[1]),
  write: (text, v) => OURS.reduce((t, n) => t.replace(new RegExp(`(name = "${n}"\\r?\\nversion = ")[^"]+(")`), `$1${v}$2`), text),
};
const shellDeps = {
  file: "src-tauri/Cargo.toml",
  read: (text) => [...text.matchAll(/^tunekore-\w+ = \{ version = "([^"]+)"/gm)].map((m) => m[1]),
  write: (text, v) => text.replace(/^(tunekore-\w+ = \{ version = ")[^"]+(")/gm, `$1${v}$2`),
};

const PLACES = [
  json("package.json", ["version"]),
  json("package-lock.json", ["version"], ["packages", "", "version"]),
  json("src-tauri/tauri.conf.json", ["version"]),
  cargoPackage("src-tauri/Cargo.toml"),
  cargoPackage("engine/Cargo.toml"),
  cargoPackage("analyzer/Cargo.toml"),
  shellDeps,
  cargoLock,
];

function current() {
  const found = PLACES.flatMap((p) => p.read(readFileSync(p.file, "utf8")).map((v) => [p.file, v]));
  const versions = new Set(found.map(([, v]) => v));
  if (versions.size !== 1 || !SEMVER.test([...versions][0])) {
    for (const [file, v] of found) console.error(`  ${file}: ${v}`);
    throw new Error("the version copies disagree; set them with `node scripts/version.mjs <x.y.z>`");
  }
  return [...versions][0];
}

function next(v, part) {
  const [major, minor, patch] = v.split(".").map(Number);
  if (part === "major") return `${major + 1}.0.0`;
  if (part === "minor") return `${major}.${minor + 1}.0`;
  return `${major}.${minor}.${patch + 1}`;
}

const arg = process.argv[2];
try {
  const now = current();
  if (!arg) {
    console.log(now);
  } else {
    const to = ["major", "minor", "patch"].includes(arg) ? next(now, arg) : arg;
    if (!SEMVER.test(to)) throw new Error(`not a version: ${to}`);
    for (const p of PLACES) writeFileSync(p.file, p.write(readFileSync(p.file, "utf8"), to));
    console.log(`${now} -> ${current()}`);
    if (!readFileSync("CHANGELOG.md", "utf8").includes(`## ${to}`)) console.log(`Now add a "## ${to}" entry to CHANGELOG.md.`);
  }
} catch (e) {
  console.error(e.message);
  process.exit(1);
}
