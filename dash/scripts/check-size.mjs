// The first screen's JavaScript, compressed, must stay under 300 KB
// (RFC 0003 §9): the scripts index.html loads and preloads.
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { gzipSync } from "node:zlib";

const LIMIT = 300 * 1024;
const dist = new URL("../dist/", import.meta.url).pathname;
const html = readFileSync(join(dist, "index.html"), "utf8");
const files = [...html.matchAll(/(?:src|href)="\/(assets\/[^"]+\.js)"/g)].map((m) => m[1]);
let total = 0;
for (const f of files) {
  const size = gzipSync(readFileSync(join(dist, f)), { level: 9 }).length;
  total += size;
  console.log(`${(size / 1024).toFixed(1).padStart(7)} KB  ${f}`);
}
console.log(`${(total / 1024).toFixed(1).padStart(7)} KB  first screen, compressed (limit ${LIMIT / 1024} KB)`);
if (total > LIMIT) process.exit(1);
