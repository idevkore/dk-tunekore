// Fails if any Rust crate or npm package in the lockfiles has no permissive
// licence option. Run: npm run licenses  (needs cargo on PATH).
//
// An SPDX expression passes when at least one OR-branch has only allowed terms,
// so "MIT OR Apache-2.0 OR LGPL-2.1-or-later" passes (we take MIT) and a bare
// "LGPL-2.1" or "GPL-3.0" does not. See CLAUDE.md "Own the IP".
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

const ALLOWED = new Set([
  "MIT", "MIT-0", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Zlib",
  "Unlicense", "CC0-1.0", "0BSD", "BSL-1.0", "Unicode-3.0", "Unicode-DFS-2016",
  "MPL-2.0", // fine for unmodified files (CLAUDE.md)
]);

// Tiny SPDX parser: returns true if the expression can be satisfied by ALLOWED.
function ok(expr) {
  const tokens = expr
    .replace(/\//g, " OR ")
    .replace(/[()]/g, (c) => ` ${c} `)
    .split(/\s+/)
    .filter(Boolean);
  let i = 0;
  const parseOr = () => {
    let v = parseAnd();
    while (tokens[i] === "OR") { i++; const r = parseAnd(); v = v || r; }
    return v;
  };
  const parseAnd = () => {
    let v = parseTerm();
    while (tokens[i] === "AND") { i++; const r = parseTerm(); v = v && r; }
    return v;
  };
  const parseTerm = () => {
    if (tokens[i] === "(") { i++; const v = parseOr(); i++; return v; }
    const id = tokens[i++].replace(/\+$/, "");
    if (tokens[i] === "WITH") i += 2; // exceptions only widen a licence
    return ALLOWED.has(id);
  };
  return parseOr();
}

const found = new Map(); // licence expression -> count
const bad = [];
const note = (kind, name, version, license) => {
  const lic = license || "NONE";
  found.set(lic, (found.get(lic) ?? 0) + 1);
  if (!license || !ok(license)) bad.push(`${kind} ${name}@${version}: ${lic}`);
};

const meta = JSON.parse(
  execFileSync("cargo", ["metadata", "--format-version", "1", "--locked"], {
    cwd: join(root, "src-tauri"), maxBuffer: 1 << 28,
  }),
);
for (const p of meta.packages) {
  if (p.source) note("crate", p.name, p.version, p.license); // skip our own crate
}

const lock = JSON.parse(readFileSync(join(root, "package-lock.json"), "utf8"));
for (const [path, p] of Object.entries(lock.packages)) {
  if (path) note("npm", path.replace(/^.*node_modules\//, ""), p.version, p.license);
}

if (process.argv.includes("--list")) {
  for (const [lic, n] of [...found].sort((a, b) => b[1] - a[1])) console.log(String(n).padStart(4), lic);
}
if (bad.length) {
  console.error("Licences outside the allowlist:\n" + bad.join("\n"));
  process.exit(1);
}
console.log(`licence check ok: ${[...found.values()].reduce((a, b) => a + b, 0)} packages`);
