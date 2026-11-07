# Vendored Cloudflare workers

Unmodified Cloudflare code from [workers-sdk](https://github.com/cloudflare/workers-sdk), built as
miniflare ships it:

- `asset-worker.js` and `router-worker.js`: the Workers Static Assets workers
  (`packages/workers-shared`; miniflare's `dist/src/workers/assets/`).
- `workflows-engine.js`: the Workflows engine, a Durable Object per instance, and its
  `WorkflowBinding` entrypoint (`packages/workflows-shared`; miniflare's
  `dist/src/workers/workflows/binding.worker.js`).
- `zod.js`: the zod build the engine imports as `miniflare:zod` (miniflare's
  `dist/src/workers/shared/zod.worker.js`).

- Source: npm `miniflare@5.20261104.0-beta` (a dependency of the `wrangler` pinned in
  `test/e2e/package.json`)
- License: MIT, see `LICENSE`. `zod.js` bundles [zod](https://github.com/colinhacks/zod), also MIT (Copyright (c) 2025 Colin McDonnell)

To update them, bump `wrangler` in `test/e2e/package.json`, run `npm ci` there, then:

```sh
cp test/e2e/node_modules/miniflare/dist/src/workers/assets/assets.worker.js crates/selvedge-workerd/src/vendor/asset-worker.js
cp test/e2e/node_modules/miniflare/dist/src/workers/assets/router.worker.js crates/selvedge-workerd/src/vendor/router-worker.js
cp test/e2e/node_modules/miniflare/dist/src/workers/workflows/binding.worker.js crates/selvedge-workerd/src/vendor/workflows-engine.js
cp test/e2e/node_modules/miniflare/dist/src/workers/shared/zod.worker.js crates/selvedge-workerd/src/vendor/zod.js
```

and update the version above. The end-to-end tests exercise them all.
