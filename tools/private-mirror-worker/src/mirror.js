// Private CI file mirror backed by Cloudflare R2.
//
// Serves vendor files CI needs but cannot fetch from the vendor directly
// (captcha-gated downloads with no scriptable URL — e.g. SVBony's Windows
// SDK, docs/services/svbony-camera.md § Windows). The mirror is private, not
// published: EVERY request, reads included, must present the bearer token, so
// the bucket's contents are never exposed to anyone but our own CI — the
// opposite of the bazel cache worker's anonymous-read model.
//
// Read-only by design: there is no PUT path at all. Uploads happen
// out-of-band via `wrangler r2 object put` from an operator's
// authenticated wrangler session, so the public-facing surface can never
// mutate the bucket regardless of token compromise.
//
// Bindings (wrangler.toml): FILES = R2 bucket.  Secret: READ_TOKEN.

// Object keys: path segments of word/dot/dash chars. Rejects empty keys,
// dotfiles, `..`, and consecutive/trailing slashes by construction.
const KEY_RE = /^([\w-][\w.-]*\/)*[\w-][\w.-]*$/;

export default {
  async fetch(request, env) {
    // Auth before anything else — an unauthenticated caller learns nothing,
    // not even whether a key exists (no public health endpoint either).
    if ((request.headers.get("Authorization") || "") !== `Bearer ${env.READ_TOKEN}`) {
      return new Response("forbidden\n", { status: 403 });
    }

    const key = new URL(request.url).pathname.replace(/^\/+/, "");
    if (!KEY_RE.test(key)) {
      return new Response("not a mirror key\n", { status: 400 });
    }

    switch (request.method) {
      case "GET": {
        const obj = await env.FILES.get(key);
        if (!obj) return new Response(null, { status: 404 });
        // Content-Length gives curl/CI a sized body instead of a chunked
        // stream; the body streams straight from R2, no buffering.
        return new Response(obj.body, {
          status: 200,
          headers: { "Content-Length": String(obj.size) },
        });
      }
      case "HEAD": {
        const obj = await env.FILES.head(key);
        return new Response(null, { status: obj ? 200 : 404 });
      }
      default:
        return new Response("method not allowed\n", { status: 405 });
    }
  },
};
