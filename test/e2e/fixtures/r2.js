// Exercises the R2 binding; GET /run returns the results as JSON.
export default {
  async fetch(req, env) {
    const B = env.BUCKET;
    const out = {};
    const check = (name, v) => (out[name] = v);
    await B.put("a/one.txt", "hello world", { httpMetadata: { contentType: "text/plain" }, customMetadata: { who: "me" } });
    const got = await B.get("a/one.txt");
    check("get", await got.text());
    check("contentType", got.httpMetadata.contentType);
    check("custom", got.customMetadata.who);
    check("etagIsMd5", got.etag === "5eb63bbbe01eeed093cb22bb8f5acdc3");
    check("head.size", (await B.head("a/one.txt")).size);
    check("range", await (await B.get("a/one.txt", { range: { offset: 6, length: 5 } })).text());
    check("suffix", await (await B.get("a/one.txt", { range: { suffix: 3 } })).text());
    check("missing", await B.get("nope"));
    await B.put("a/two.txt", "2");
    await B.put("b/three.txt", "3");
    await B.put("top.txt", "t");
    const listed = await B.list({ delimiter: "/" });
    check("list.objects", listed.objects.map((o) => o.key));
    check("list.prefixes", listed.delimitedPrefixes);
    check("list.prefix", (await B.list({ prefix: "a/" })).objects.map((o) => o.key));
    const page = await B.list({ limit: 2 });
    check("page1", [page.objects.map((o) => o.key), page.truncated]);
    const page2 = await B.list({ limit: 2, cursor: page.cursor });
    check("page2", page2.objects.map((o) => o.key));
    check("onlyIfMatchFails", await B.put("a/one.txt", "x", { onlyIf: { etagMatches: "nope" } }));
    check("onlyIfNoneMatchCreates", (await B.put("new.txt", "n", { onlyIf: { etagDoesNotMatch: "*" } })) !== null);
    check("onlyIfNoneMatchBlocks", await B.put("new.txt", "n2", { onlyIf: { etagDoesNotMatch: "*" } }));
    const cond = await B.get("a/one.txt", { onlyIf: { etagDoesNotMatch: got.etag } });
    check("conditionalGetHasNoBody", cond !== null && !("body" in cond));
    // Multipart: two parts, the first at the 5 MiB minimum.
    const up = await B.createMultipartUpload("big.bin", { httpMetadata: { contentType: "application/x-test" } });
    const p1 = await up.uploadPart(1, new Uint8Array(5 * 1024 * 1024).fill(97));
    const p2 = await up.uploadPart(2, "tail");
    const done = await up.complete([p1, p2]);
    check("multipart.size", done.size);
    check("multipart.etag", /^[0-9a-f]{32}-2$/.test(done.etag));
    check("multipart.tail", await (await B.get("big.bin", { range: { suffix: 6 } })).text());
    const small = await B.createMultipartUpload("small.bin");
    const s1 = await small.uploadPart(1, "too small");
    const s2 = await small.uploadPart(2, "x");
    check("multipart.minPartSize", await small.complete([s1, s2]).then(() => "accepted", (e) => e.message.includes("10011")));
    await small.abort();
    await B.delete(["a/two.txt", "b/three.txt"]);
    check("afterDelete", (await B.list()).objects.map((o) => o.key));
    return Response.json(out);
  },
};
