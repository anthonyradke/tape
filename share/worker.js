// tape's share server: takes clips from the app in parts, keeps them in R2 for LINK_DAYS, and
// serves each at /<id> as a page that plays it, with the tags chat apps need to embed the video.
// The app's requests carry UPLOAD_KEY; everything else is read-only and only finds unexpired clips.

const PART_BYTES = 20 * 1024 * 1024; // all parts but the last must be the same size
const MAX_CLIP = 2e9;
const MAX_THUMB = 2 * 1024 * 1024;
const ID = /^[A-Za-z0-9]{10}$/;
const DAY = 86400000;

const video = (id) => `c/${id}.mp4`;
const thumb = (id) => `c/${id}.jpg`;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const parts = url.pathname.split("/").filter(Boolean);
    try {
      if (parts[0] === "api") {
        if (!authorized(request, env)) return json({ error: "Wrong upload key." }, 401);
        return await api(request, env, url, parts.slice(1));
      }
      if (request.method !== "GET" && request.method !== "HEAD") return new Response(null, { status: 405 });
      if (parts.length !== 1) return page(env, null, 404);
      const [, id, ext] = parts[0].match(/^([^.]+)(?:\.(mp4|jpg))?$/) || [];
      if (!ID.test(id || "")) return page(env, null, 404);
      if (ext) return file(request, env, ext === "mp4" ? video(id) : thumb(id), id);
      return watch(request, env, url, id);
    } catch (err) {
      return json({ error: String(err?.message || err) }, 500);
    }
  },
};

// Constant-time, so the key can't be guessed a character at a time.
function authorized(request, env) {
  const given = new TextEncoder().encode(request.headers.get("authorization") || "");
  const wanted = new TextEncoder().encode(`Bearer ${env.UPLOAD_KEY || ""}`);
  if (!env.UPLOAD_KEY || given.length !== wanted.length) return false;
  let diff = 0;
  for (let i = 0; i < given.length; i++) diff |= given[i] ^ wanted[i];
  return diff === 0;
}

const json = (body, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", "cache-control": "no-store" } });

function newId() {
  const abc = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
  const bytes = crypto.getRandomValues(new Uint8Array(10));
  return [...bytes].map((b) => abc[b % abc.length]).join("");
}

const expired = (object) => Number(object?.customMetadata?.expires || 0) < Date.now();

async function remove(env, id) {
  await env.BUCKET.delete([video(id), thumb(id)]);
}

// Everything stored, with expired clips deleted on the way: the bucket's own rule only runs once a
// day, and space they hold would otherwise count against the limit.
async function usage(env, origin) {
  const clips = [];
  let used = 0;
  let cursor;
  do {
    const page = await env.BUCKET.list({ prefix: "c/", cursor, include: ["customMetadata"] });
    for (const o of page.objects) {
      const id = o.key.slice(2, -4);
      if (o.key.endsWith(".mp4") && expired(o)) {
        await remove(env, id);
        continue;
      }
      used += o.size;
      if (!o.key.endsWith(".mp4")) continue;
      const m = o.customMetadata || {};
      clips.push({ id, url: `${origin}/${id}`, name: m.name || "", source: m.source || "", bytes: o.size, seconds: Number(m.seconds || 0), uploaded: o.uploaded.getTime(), expires: Number(m.expires) });
    }
    cursor = page.truncated ? page.cursor : undefined;
  } while (cursor);
  clips.sort((a, b) => b.uploaded - a.uploaded);
  const free = Number(env.FREE_BYTES);
  return { used, free, cap: Math.floor((free * Number(env.CAP_PERCENT)) / 100), linkDays: Number(env.LINK_DAYS), clips };
}

async function api(request, env, url, parts) {
  const { method } = request;
  const [what, id, sub, n] = parts;
  if (what === "usage" && method === "GET") return json(await usage(env, url.origin));
  if (what !== "clips") return json({ error: "Not found." }, 404);

  // Start an upload: checks there's room, then hands back where to send the parts.
  if (!id && method === "POST") {
    const info = await request.json();
    const bytes = Number(info.bytes);
    if (!(bytes > 0) || bytes > MAX_CLIP) return json({ error: "Clips over 2 GB can't be shared." }, 413);
    const now = await usage(env, url.origin);
    if (now.used + bytes > now.cap) {
      return json({ error: "Not enough free cloud storage for this clip. Delete a shared link or wait for one to expire.", used: now.used, cap: now.cap }, 507);
    }
    const clip = newId();
    const expires = Date.now() + Number(env.LINK_DAYS) * DAY;
    const meta = {
      name: String(info.name || "").slice(0, 200),
      source: String(info.source || "").slice(0, 64),
      seconds: String(Number(info.seconds) || 0),
      width: String(Number(info.width) || 0),
      height: String(Number(info.height) || 0),
      expires: String(expires),
    };
    const upload = await env.BUCKET.createMultipartUpload(video(clip), { httpMetadata: { contentType: "video/mp4" }, customMetadata: meta });
    return json({ id: clip, upload: upload.uploadId, partBytes: PART_BYTES, url: `${url.origin}/${clip}`, expires });
  }
  if (!ID.test(id || "")) return json({ error: "Not found." }, 404);
  const uploadId = url.searchParams.get("upload");

  if (sub === "parts" && method === "PUT" && uploadId) {
    const number = Number(n);
    if (!(number >= 1 && number <= 10000)) return json({ error: "Bad part number." }, 400);
    const part = await env.BUCKET.resumeMultipartUpload(video(id), uploadId).uploadPart(number, request.body);
    return json({ part: part.partNumber, etag: part.etag });
  }
  if (sub === "complete" && method === "POST" && uploadId) {
    const { parts: done } = await request.json();
    await env.BUCKET.resumeMultipartUpload(video(id), uploadId).complete(done.map((p) => ({ partNumber: p.part, etag: p.etag })));
    const object = await env.BUCKET.head(video(id)); // complete() doesn't return the metadata
    return json({ url: `${url.origin}/${id}`, bytes: object.size, expires: Number(object.customMetadata.expires) });
  }
  if (sub === "thumb" && method === "PUT") {
    const body = await request.arrayBuffer();
    if (body.byteLength > MAX_THUMB) return json({ error: "Thumbnail too large." }, 413);
    const head = await env.BUCKET.head(video(id));
    if (!head) return json({ error: "Not found." }, 404);
    await env.BUCKET.put(thumb(id), body, { httpMetadata: { contentType: "image/jpeg" }, customMetadata: { expires: head.customMetadata.expires } });
    return json({ ok: true });
  }
  if (!sub && method === "DELETE") {
    if (uploadId) await env.BUCKET.resumeMultipartUpload(video(id), uploadId).abort().catch(() => {});
    await remove(env, id);
    return json({ ok: true });
  }
  return json({ error: "Not found." }, 404);
}

// The video or thumbnail itself, with range requests so players can seek.
async function file(request, env, key, id) {
  const head = await env.BUCKET.head(key);
  if (!head || expired(head)) {
    if (head) await remove(env, id);
    return new Response("This clip has expired.", { status: 410 });
  }
  const object = await env.BUCKET.get(key, { range: request.headers, onlyIf: request.headers });
  const headers = new Headers();
  object.writeHttpMetadata(headers);
  headers.set("etag", object.httpEtag);
  headers.set("accept-ranges", "bytes");
  headers.set("cache-control", "public, max-age=600");
  if (!("body" in object)) return new Response(null, { status: 304, headers });
  let status = 200;
  if (object.range && request.headers.has("range")) {
    const offset = object.range.offset ?? object.size - object.range.suffix;
    const length = object.range.length ?? object.size - offset;
    headers.set("content-range", `bytes ${offset}-${offset + length - 1}/${object.size}`);
    headers.set("content-length", String(length));
    status = 206;
  } else {
    headers.set("content-length", String(object.size));
  }
  return new Response(request.method === "HEAD" ? null : object.body, { status, headers });
}

const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

function left(ms) {
  const hours = Math.max(0, Math.round(ms / 3600000));
  if (hours < 1) return "less than an hour";
  if (hours < 48) return `${hours} hour${hours === 1 ? "" : "s"}`;
  return `${Math.round(hours / 24)} days`;
}

async function watch(request, env, url, id) {
  const head = await env.BUCKET.head(video(id));
  if (!head || expired(head)) {
    if (head) await remove(env, id);
    return page(env, null, head ? 410 : 404);
  }
  return page(env, { id, origin: url.origin, meta: head.customMetadata || {}, hasThumb: !!(await env.BUCKET.head(thumb(id))) }, 200);
}

function page(env, clip, status) {
  const title = clip ? clip.meta.name.replace(/\.(DVR\.)?mp4$/i, "") || "Clip" : status === 410 ? "This clip has expired" : "Clip not found";
  let head = "";
  let body;
  if (clip) {
    const src = `${clip.origin}/${clip.id}.mp4`;
    const poster = clip.hasThumb ? `${clip.origin}/${clip.id}.jpg` : "";
    const { width = "0", height = "0" } = clip.meta;
    head = `<meta property="og:type" content="video.other">
<meta property="og:title" content="${esc(title)}">
<meta property="og:site_name" content="tape">
<meta property="og:url" content="${clip.origin}/${clip.id}">
<meta property="og:video" content="${src}">
<meta property="og:video:secure_url" content="${src}">
<meta property="og:video:type" content="video/mp4">
${width !== "0" ? `<meta property="og:video:width" content="${esc(width)}">\n<meta property="og:video:height" content="${esc(height)}">` : ""}
${poster ? `<meta property="og:image" content="${poster}">` : ""}
<meta name="twitter:card" content="player">
<meta name="twitter:player" content="${src}">
${width !== "0" ? `<meta name="twitter:player:width" content="${esc(width)}">\n<meta name="twitter:player:height" content="${esc(height)}">` : ""}`;
    body = `<video src="${src}" ${poster ? `poster="${poster}"` : ""} controls autoplay playsinline preload="metadata"></video>
<div class="bar"><h1>${esc(title)}</h1><span>Link expires in ${left(Number(clip.meta.expires) - Date.now())} · <a href="${src}" download>Download</a></span></div>`;
  } else {
    body = `<div class="gone"><h1>${esc(title)}</h1><p>${status === 410 ? "Shared links stop working after a while. Ask for a new one." : "Check the link and try again."}</p></div>`;
  }
  const html = `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(title)}</title><meta name="robots" content="noindex">
${head}
<style>
html,body{margin:0;height:100%;background:#141517;color:#e8e8ea;font:15px/1.4 system-ui,-apple-system,"Segoe UI",sans-serif}
body{display:flex;flex-direction:column}
video{flex:1;min-height:0;width:100%;background:#000}
.bar{display:flex;gap:16px;align-items:baseline;justify-content:space-between;padding:12px 18px;flex-wrap:wrap}
h1{font-size:15px;font-weight:600;margin:0}
.bar span{color:#8b8d93;font-size:13px}
a{color:#e8e8ea}
.gone{margin:auto;text-align:center;padding:24px}.gone h1{font-size:20px;margin-bottom:8px}.gone p{color:#8b8d93;margin:0}
</style></head><body>${body}</body></html>`;
  return new Response(html, { status, headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" } });
}
