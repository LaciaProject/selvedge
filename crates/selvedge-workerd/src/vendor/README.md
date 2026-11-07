# Vendored Cloudflare workers

`asset-worker.js` and `router-worker.js` are Cloudflare's Workers Static Assets
workers, the same code that serves assets on Cloudflare. They come from
`packages/workers-shared` in [workers-sdk](https://github.com/cloudflare/workers-sdk),
built as miniflare ships them (`dist/src/workers/assets/`), and are unmodified.

- Source: npm `miniflare@5.20261104.0-beta` (a dependency of the `wrangler` pinned in
  `test/e2e/package.json`)
- License: MIT, see `LICENSE`

To update them, bump `wrangler` in `test/e2e/package.json`, run `npm ci` there, then:

```sh
cp test/e2e/node_modules/miniflare/dist/src/workers/assets/assets.worker.js crates/selvedge-workerd/src/vendor/asset-worker.js
cp test/e2e/node_modules/miniflare/dist/src/workers/assets/router.worker.js crates/selvedge-workerd/src/vendor/router-worker.js
```

and update the version above. The end-to-end tests exercise both.
