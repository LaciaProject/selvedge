// R2's S3-compatible API, driven by the official AWS SDK for JavaScript.
// Usage: node s3.mjs <endpoint> <access key id> <secret access key>
// Prints one line per check: "ok <name>" or "FAIL <name>: <detail>".
import {
  S3Client, ListBucketsCommand, CreateBucketCommand, DeleteBucketCommand, HeadBucketCommand,
  PutObjectCommand, GetObjectCommand, HeadObjectCommand, DeleteObjectCommand, DeleteObjectsCommand,
  ListObjectsV2Command, CopyObjectCommand, CreateMultipartUploadCommand, UploadPartCommand,
  CompleteMultipartUploadCommand,
} from "@aws-sdk/client-s3";
import { getSignedUrl } from "@aws-sdk/s3-request-presigner";
import { Readable } from "node:stream";

const [endpoint, accessKeyId, secretAccessKey] = process.argv.slice(2);
const client = (secret = secretAccessKey) =>
  new S3Client({ endpoint, region: "auto", forcePathStyle: true, credentials: { accessKeyId, secretAccessKey: secret } });
const s3 = client();
const Bucket = "s3-e2e";
let failed = false;
async function check(name, fn) {
  try {
    const detail = await fn();
    if (detail === true) console.log(`ok   ${name}`);
    else { failed = true; console.log(`FAIL ${name}: ${JSON.stringify(detail)}`); }
  } catch (e) {
    failed = true;
    console.log(`FAIL ${name}: ${e.name}: ${e.message}`);
  }
}
const text = async (r) => await r.Body.transformToString();

await check("CreateBucket and ListBuckets", async () => {
  await s3.send(new CreateBucketCommand({ Bucket }));
  const r = await s3.send(new ListBucketsCommand({}));
  return r.Buckets.some((b) => b.Name === Bucket) || r.Buckets;
});
await check("HeadBucket", async () => (await s3.send(new HeadBucketCommand({ Bucket })), true));
await check("PutObject (aws-chunked with a checksum trailer) and GetObject with metadata", async () => {
  await s3.send(new PutObjectCommand({
    Bucket, Key: "dir/hello.txt", Body: "hello world", ContentType: "text/plain", Metadata: { owner: "e2e" },
  }));
  const r = await s3.send(new GetObjectCommand({ Bucket, Key: "dir/hello.txt" }));
  const body = await text(r);
  return (body === "hello world" && r.ContentType === "text/plain" && r.Metadata.owner === "e2e") || { body, ct: r.ContentType, md: r.Metadata };
});
await check("PutObject with a stream body", async () => {
  const chunks = ["streamed ", "in ", "pieces"];
  await s3.send(new PutObjectCommand({
    Bucket, Key: "stream.txt", Body: Readable.from(chunks.map((c) => Buffer.from(c))), ContentLength: 19,
  }));
  const body = await text(await s3.send(new GetObjectCommand({ Bucket, Key: "stream.txt" })));
  return body === "streamed in pieces" || body;
});
await check("ranged GetObject", async () => {
  const r = await s3.send(new GetObjectCommand({ Bucket, Key: "dir/hello.txt", Range: "bytes=6-10" }));
  const body = await text(r);
  return (body === "world" && r.ContentRange === "bytes 6-10/11") || { body, range: r.ContentRange };
});
await check("HeadObject and a 304 on If-None-Match", async () => {
  const h = await s3.send(new HeadObjectCommand({ Bucket, Key: "dir/hello.txt" }));
  try {
    await s3.send(new GetObjectCommand({ Bucket, Key: "dir/hello.txt", IfNoneMatch: h.ETag }));
    return "no 304";
  } catch (e) {
    return (h.ContentLength === 11 && e.$metadata?.httpStatusCode === 304) || { len: h.ContentLength, status: e.$metadata?.httpStatusCode };
  }
});
await check("conditional PutObject (If-None-Match: *) refuses an overwrite", async () => {
  try {
    await s3.send(new PutObjectCommand({ Bucket, Key: "dir/hello.txt", Body: "again", IfNoneMatch: "*" }));
    return "overwrote";
  } catch (e) {
    return e.$metadata?.httpStatusCode === 412 || e.$metadata?.httpStatusCode;
  }
});
await check("ListObjectsV2 with a delimiter and pagination", async () => {
  for (const k of ["a.txt", "b.txt", "c.txt"]) await s3.send(new PutObjectCommand({ Bucket, Key: k, Body: k }));
  const top = await s3.send(new ListObjectsV2Command({ Bucket, Delimiter: "/" }));
  const keys = [];
  let token, pages = 0;
  do {
    const p = await s3.send(new ListObjectsV2Command({ Bucket, MaxKeys: 2, ContinuationToken: token }));
    keys.push(...(p.Contents ?? []).map((o) => o.Key));
    token = p.IsTruncated ? p.NextContinuationToken : undefined;
    pages++;
  } while (token && pages < 10);
  const want = ["a.txt", "b.txt", "c.txt", "dir/hello.txt", "stream.txt"];
  const ok = top.CommonPrefixes?.[0]?.Prefix === "dir/" && top.Contents.length === 4 && pages === 3 && keys.join() === want.join();
  return ok || { prefixes: top.CommonPrefixes, top: top.Contents.length, keys, pages };
});
await check("CopyObject", async () => {
  await s3.send(new CopyObjectCommand({ Bucket, Key: "copy.txt", CopySource: `${Bucket}/dir/hello.txt` }));
  const r = await s3.send(new GetObjectCommand({ Bucket, Key: "copy.txt" }));
  const body = await text(r);
  return (body === "hello world" && r.Metadata.owner === "e2e") || { body, md: r.Metadata };
});
await check("multipart upload", async () => {
  const { UploadId } = await s3.send(new CreateMultipartUploadCommand({ Bucket, Key: "big.bin", ContentType: "application/x-test" }));
  const part1 = Buffer.alloc(5 * 1024 * 1024, 97);
  const part2 = Buffer.from("tail");
  const e1 = (await s3.send(new UploadPartCommand({ Bucket, Key: "big.bin", UploadId, PartNumber: 1, Body: part1 }))).ETag;
  const e2 = (await s3.send(new UploadPartCommand({ Bucket, Key: "big.bin", UploadId, PartNumber: 2, Body: part2 }))).ETag;
  const done = await s3.send(new CompleteMultipartUploadCommand({
    Bucket, Key: "big.bin", UploadId, MultipartUpload: { Parts: [{ PartNumber: 1, ETag: e1 }, { PartNumber: 2, ETag: e2 }] },
  }));
  const h = await s3.send(new HeadObjectCommand({ Bucket, Key: "big.bin" }));
  const tail = await text(await s3.send(new GetObjectCommand({ Bucket, Key: "big.bin", Range: "bytes=-4" })));
  return (h.ContentLength === part1.length + 4 && done.ETag.endsWith('-2"') && tail === "tail" && h.ContentType === "application/x-test") ||
    { len: h.ContentLength, etag: done.ETag, tail, ct: h.ContentType };
});
await check("presigned GET and PUT", async () => {
  const put = await getSignedUrl(s3, new PutObjectCommand({ Bucket, Key: "presigned.txt" }), { expiresIn: 300 });
  const pr = await fetch(put, { method: "PUT", body: "via presigned url" });
  const get = await getSignedUrl(s3, new GetObjectCommand({ Bucket, Key: "presigned.txt" }), { expiresIn: 300 });
  const gr = await fetch(get);
  const tampered = await fetch(get.replace("presigned.txt", "a.txt"));
  const body = await gr.text();
  return (pr.status === 200 && body === "via presigned url" && tampered.status === 403) || { put: pr.status, body, tampered: tampered.status };
});
await check("a wrong secret is refused", async () => {
  try {
    await client("not-the-secret").send(new ListBucketsCommand({}));
    return "accepted";
  } catch (e) {
    return e.name === "SignatureDoesNotMatch" || e.name;
  }
});
await check("DeleteObjects, DeleteObject and DeleteBucket", async () => {
  const keys = ["a.txt", "b.txt", "c.txt", "copy.txt", "big.bin", "presigned.txt", "stream.txt"];
  const r = await s3.send(new DeleteObjectsCommand({ Bucket, Delete: { Objects: keys.map((Key) => ({ Key })) } }));
  await s3.send(new DeleteObjectCommand({ Bucket, Key: "dir/hello.txt" }));
  await s3.send(new DeleteBucketCommand({ Bucket }));
  const left = await s3.send(new ListBucketsCommand({}));
  return (r.Deleted.length === keys.length && !left.Buckets?.some((b) => b.Name === Bucket)) || { deleted: r.Deleted?.length };
});
process.exit(failed ? 1 : 0);
