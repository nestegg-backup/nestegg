// Licence check of all installed npm packages (dependencies and build tools).
// Fails if a licence is not on the AGPL-3.0-compatible allow list.
// Usage: npm run licenses            (summary)
//        npm run licenses -- --list  (every package)

import { readdirSync, readFileSync, existsSync } from "node:fs";
import { join } from "node:path";

const ALLOWED = new Set([
  "MIT",
  "MIT-0",
  "ISC",
  "BSD-2-Clause",
  "BSD-3-Clause",
  "0BSD",
  "Apache-2.0",
  "MPL-2.0",
  "OFL-1.1",
  "CC0-1.0",
  "CC-BY-4.0", // data only (caniuse-lite)
  "BlueOak-1.0.0",
  "Python-2.0",
  "Unlicense",
  "GPL-3.0-or-later",
  "AGPL-3.0-or-later",
  "LGPL-3.0-or-later",
]);

const root = join(import.meta.dirname, "..", "node_modules");
/** @type {Map<string, string>} */
const found = new Map();

/** @param {{ license?: unknown, licenses?: unknown }} pkg @returns {string} */
function licenceOf(pkg) {
  let l = pkg.license ?? pkg.licenses;
  if (Array.isArray(l)) l = l.map((/** @type {any} */ x) => x.type ?? x).join(" OR ");
  if (l && typeof l === "object") l = /** @type {{ type?: string }} */ (l).type;
  return typeof l === "string" ? l : "UNKNOWN";
}

/** "(MIT OR Apache-2.0)" is fine if one alternative is allowed; "A AND B" needs both. */
/** @param {string} expr @returns {boolean} */
function allowed(expr) {
  const e = expr.replace(/[()]/g, "").trim();
  if (e.includes(" OR ")) return e.split(" OR ").some((/** @type {string} */ x) => allowed(x));
  if (e.includes(" AND ")) return e.split(" AND ").every((/** @type {string} */ x) => allowed(x));
  return ALLOWED.has(e);
}

/** @param {string} dir */
function walk(dir) {
  if (!existsSync(dir)) return;
  for (const name of readdirSync(dir)) {
    if (name.startsWith(".")) continue;
    const p = join(dir, name);
    if (name.startsWith("@")) {
      walk(p);
      continue;
    }
    const manifest = join(p, "package.json");
    if (existsSync(manifest)) {
      const pkg = JSON.parse(readFileSync(manifest, "utf8"));
      if (pkg.name && pkg.version) found.set(`${pkg.name}@${pkg.version}`, licenceOf(pkg));
      walk(join(p, "node_modules"));
    }
  }
}

walk(root);
/** @type {Map<string, number>} */
const byLicence = new Map();
const bad = [];
for (const [id, l] of found) {
  byLicence.set(l, (byLicence.get(l) ?? 0) + 1);
  if (!allowed(l)) bad.push(`${id}: ${l}`);
}
if (process.argv.includes("--list")) for (const [id, l] of [...found].sort()) console.log(`${l}\t${id}`);
console.log(`${found.size} packages`);
for (const [l, n] of [...byLicence].sort((a, b) => b[1] - a[1])) console.log(`  ${String(n).padStart(4)}  ${l}`);
if (bad.length) {
  console.error(`\nNot on the allow list:\n  ${bad.join("\n  ")}`);
  process.exit(1);
}
console.log("All licences are AGPL-3.0-compatible.");
