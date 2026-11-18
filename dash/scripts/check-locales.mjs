// The locale files must agree (RFC 0003 §8): every key the code uses exists
// in English, and Chinese has exactly English's keys.
//   node scripts/check-locales.mjs           check
//   node scripts/check-locales.mjs --used    print the keys the code uses
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

const root = new URL("../src/", import.meta.url).pathname;
const load = (f) => JSON.parse(readFileSync(join(root, "i18n", f), "utf8"));
const flatten = (o, p = "") =>
  Object.entries(o).flatMap(([k, v]) => (typeof v === "object" && v !== null ? flatten(v, `${p}${k}.`) : [`${p}${k}`]));

function files(dir) {
  return readdirSync(dir).flatMap((f) => {
    const p = join(dir, f);
    return statSync(p).isDirectory() ? files(p) : /\.tsx?$/.test(f) && !f.endsWith(".test.ts") ? [p] : [];
  });
}

// t("a.b"), t('a.b'), i18nKey="a.b"; and `prefix.${x}` templates, whose
// prefix must exist as a group.
const used = new Set();
const prefixes = new Set();
for (const f of files(root)) {
  const src = readFileSync(f, "utf8");
  for (const m of src.matchAll(/\bt\(\s*["']([a-zA-Z0-9_.]+)["']/g)) used.add(m[1]);
  for (const m of src.matchAll(/i18nKey=["']([a-zA-Z0-9_.]+)["']/g)) used.add(m[1]);
  for (const m of src.matchAll(/\bt\(\s*`([a-zA-Z0-9_.]+)\.\$\{/g)) prefixes.add(m[1]);
  // Keys kept in data (label: "nav.workers") and passed to t() later.
  for (const m of src.matchAll(/(?:label|key):\s*"([a-z][a-zA-Z0-9]*\.[a-zA-Z0-9_.]+)"/g)) used.add(m[1]);
}
// Error keys come from lib/errors.ts's table.
for (const m of readFileSync(join(root, "lib/errors.ts"), "utf8").matchAll(/"(errors\.[a-zA-Z]+)"/g)) used.add(m[1]);

if (process.argv.includes("--used")) {
  console.log([...used].sort().join("\n"));
  console.log([...prefixes].sort().map((p) => `${p}.*`).join("\n"));
  process.exit(0);
}

const en = new Set(flatten(load("en.json")));
const zh = new Set(flatten(load("zh-CN.json")));
const problems = [];
const has = (set, k) => set.has(k) || set.has(`${k}_one`) || set.has(`${k}_other`);
for (const k of used) if (!has(en, k)) problems.push(`en.json lacks ${k}`);
for (const p of prefixes) if (![...en].some((k) => k.startsWith(`${p}.`))) problems.push(`en.json lacks the group ${p}`);
for (const k of en) if (!zh.has(k)) problems.push(`zh-CN.json lacks ${k}`);
for (const k of zh) if (!en.has(k)) problems.push(`zh-CN.json has ${k}, which en.json does not`);
if (problems.length) {
  console.error(problems.join("\n"));
  process.exit(1);
}
console.log(`locales agree: ${en.size} keys, ${used.size} used in the code`);
