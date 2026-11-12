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
// One per upload in progress: the size it declared, so parts past that are refused and space it
// will take counts against the cap before it's finished. Gone once it completes or is cancelled.
const pending = (id) => `u/${id}`;

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
      if (parts.length !== 1) return await page(env, null, 404);
      const [, id, ext] = parts[0].match(/^([^.]+)(?:\.(mp4|jpg))?$/) || [];
      if (!ID.test(id || "")) return await page(env, null, 404);
      if (ext) return await file(request, env, ext === "mp4" ? video(id) : thumb(id), id);
      return await watch(request, env, url, id);
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
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", "cache-control": "no-store", "x-content-type-options": "nosniff" } });

function newId() {
  const abc = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
  const bytes = crypto.getRandomValues(new Uint8Array(10));
  return [...bytes].map((b) => abc[b % abc.length]).join("");
}

const expired = (object) => Number(object?.customMetadata?.expires || 0) < Date.now();

async function remove(env, id) {
  await env.BUCKET.delete([video(id), thumb(id), pending(id)]);
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
  // Uploads still going hold their declared size. The bucket aborts an upload after a day, so a
  // marker older than that belongs to one that was abandoned.
  let reserved = 0;
  do {
    const page = await env.BUCKET.list({ prefix: "u/", cursor, include: ["customMetadata"] });
    for (const o of page.objects) {
      const m = o.customMetadata || {};
      if (Number(m.started || 0) < Date.now() - DAY) await env.BUCKET.delete(o.key);
      else reserved += Number(m.bytes || 0);
    }
    cursor = page.truncated ? page.cursor : undefined;
  } while (cursor);
  clips.sort((a, b) => b.uploaded - a.uploaded);
  const free = Number(env.FREE_BYTES);
  return { used: used + reserved, reserved, free, cap: Math.floor((free * Number(env.CAP_PERCENT)) / 100), linkDays: Number(env.LINK_DAYS), clips };
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
      game: String(info.game || "").slice(0, 100), // optional, for the preview's description
      source: String(info.source || "").slice(0, 64),
      seconds: String(Number(info.seconds) || 0),
      width: String(Number(info.width) || 0),
      height: String(Number(info.height) || 0),
      expires: String(expires),
    };
    const upload = await env.BUCKET.createMultipartUpload(video(clip), { httpMetadata: { contentType: "video/mp4" }, customMetadata: meta });
    await env.BUCKET.put(pending(clip), "", { customMetadata: { bytes: String(bytes), upload: upload.uploadId, started: String(Date.now()) } });
    return json({ id: clip, upload: upload.uploadId, partBytes: PART_BYTES, url: `${url.origin}/${clip}`, expires });
  }
  if (!ID.test(id || "")) return json({ error: "Not found." }, 404);
  const uploadId = url.searchParams.get("upload");

  if (sub === "parts" && method === "PUT" && uploadId) {
    const declared = await declaredBytes(env, id, uploadId);
    if (declared == null) return json({ error: "No such upload." }, 404);
    // No more parts than the declared size needs, each no bigger than a part.
    const number = Number(n);
    if (!(Number.isInteger(number) && number >= 1 && number <= Math.ceil(declared / PART_BYTES))) return json({ error: "Bad part number." }, 400);
    const length = Number(request.headers.get("content-length"));
    if (!(length > 0 && length <= PART_BYTES)) return json({ error: "Parts need a length of at most 20 MiB." }, 413);
    const part = await env.BUCKET.resumeMultipartUpload(video(id), uploadId).uploadPart(number, request.body);
    return json({ part: part.partNumber, etag: part.etag });
  }
  if (sub === "complete" && method === "POST" && uploadId) {
    const declared = await declaredBytes(env, id, uploadId);
    if (declared == null) return json({ error: "No such upload." }, 404);
    const { parts: done } = await request.json();
    await env.BUCKET.resumeMultipartUpload(video(id), uploadId).complete(done.map((p) => ({ partNumber: p.part, etag: p.etag })));
    const object = await env.BUCKET.head(video(id)); // complete() doesn't return the metadata
    // Parts can't be measured before completing, so a file larger than it said is removed after.
    if (!object || object.size > declared) {
      await remove(env, id);
      return json({ error: "The upload was larger than the size it started with." }, 413);
    }
    await env.BUCKET.delete(pending(id));
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

// The size an upload in progress declared, or null if there's no such upload.
async function declaredBytes(env, id, uploadId) {
  const marker = await env.BUCKET.head(pending(id));
  if (!marker || marker.customMetadata?.upload !== uploadId) return null;
  return Number(marker.customMetadata.bytes);
}

// The video or thumbnail itself, with range requests so players can seek. Conditional headers
// and ranges are worked out here from the object's head rather than handed to R2, so a failed
// precondition answers 412 or 304 as HTTP says, and a range past the end 416 instead of an error.
async function file(request, env, key, id) {
  const head = await env.BUCKET.head(key);
  if (!head || expired(head)) {
    if (head) await remove(env, id);
    return new Response("This clip has expired.", { status: 410, headers: { "x-content-type-options": "nosniff" } });
  }
  const headers = new Headers();
  head.writeHttpMetadata(headers);
  headers.set("etag", head.httpEtag);
  headers.set("last-modified", head.uploaded.toUTCString());
  headers.set("accept-ranges", "bytes");
  headers.set("cache-control", "public, max-age=600");
  headers.set("x-content-type-options", "nosniff");

  const condition = precondition(request.headers, head);
  if (condition) return new Response(null, { status: condition, headers });

  const range = parseRange(request.headers, head);
  if (range === "unsatisfiable") {
    headers.set("content-range", `bytes */${head.size}`);
    return new Response(null, { status: 416, headers });
  }
  if (request.method === "HEAD") {
    if (range) {
      headers.set("content-range", `bytes ${range.offset}-${range.offset + range.length - 1}/${head.size}`);
      headers.set("content-length", String(range.length));
    } else {
      headers.set("content-length", String(head.size));
    }
    return new Response(null, { status: range ? 206 : 200, headers });
  }
  const object = await env.BUCKET.get(key, range ? { range } : {});
  if (!object) return new Response("This clip has expired.", { status: 410, headers: { "x-content-type-options": "nosniff" } });
  if (range) {
    headers.set("content-range", `bytes ${range.offset}-${range.offset + range.length - 1}/${object.size}`);
    headers.set("content-length", String(range.length));
    return new Response(object.body, { status: 206, headers });
  }
  headers.set("content-length", String(object.size));
  return new Response(object.body, { status: 200, headers });
}

const tags = (value) => value.split(",").map((t) => t.trim()).filter(Boolean);
const sameTag = (tag, etag, weak) => tag === "*" || (weak ? tag.replace(/^W\//, "") : tag) === etag;
const seconds = (date) => Math.floor(date.getTime() / 1000);

// RFC 9110's order: If-Match, else If-Unmodified-Since (412 when they fail), then If-None-Match,
// else If-Modified-Since (304 when they fail). Null when the request should go ahead.
function precondition(h, head) {
  const etag = head.httpEtag;
  const modified = seconds(head.uploaded);
  if (h.has("if-match")) {
    if (!tags(h.get("if-match")).some((t) => !t.startsWith("W/") && sameTag(t, etag, false))) return 412;
  } else if (h.has("if-unmodified-since")) {
    const since = Date.parse(h.get("if-unmodified-since"));
    if (!Number.isNaN(since) && modified > since / 1000) return 412;
  }
  if (h.has("if-none-match")) {
    if (tags(h.get("if-none-match")).some((t) => sameTag(t, etag, true))) return 304;
  } else if (h.has("if-modified-since")) {
    const since = Date.parse(h.get("if-modified-since"));
    if (!Number.isNaN(since) && modified <= since / 1000) return 304;
  }
  return null;
}

// One "bytes=" range as { offset, length }, null for the whole file (no Range, several ranges, one
// that doesn't parse, or an If-Range that no longer matches), or "unsatisfiable".
function parseRange(h, head) {
  const value = h.get("range");
  if (!value) return null;
  const ifRange = h.get("if-range");
  if (ifRange && (ifRange.startsWith('"') || ifRange.startsWith("W/") ? ifRange !== head.httpEtag : Date.parse(ifRange) / 1000 < seconds(head.uploaded))) return null;
  const m = value.trim().match(/^bytes=(\d*)-(\d*)$/);
  if (!m || (m[1] === "" && m[2] === "")) return null;
  const size = head.size;
  if (m[1] === "") {
    const suffix = Number(m[2]);
    if (suffix === 0 || size === 0) return "unsatisfiable";
    const length = Math.min(suffix, size);
    return { offset: size - length, length };
  }
  const offset = Number(m[1]);
  if (offset >= size) return "unsatisfiable";
  const last = m[2] === "" ? size - 1 : Math.min(Number(m[2]), size - 1);
  if (last < offset) return null; // not a valid range, so it's ignored
  return { offset, length: last - offset + 1 };
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

function clock(seconds) {
  const s = Math.round(seconds);
  const pad = (n) => String(n).padStart(2, "0");
  return s >= 3600 ? `${Math.floor(s / 3600)}:${pad(Math.floor((s % 3600) / 60))}:${pad(s % 60)}` : `${Math.floor(s / 60)}:${pad(s % 60)}`;
}

// "A 0:45 clip from Fortnite", with whichever of the two the upload gave.
function describe(meta) {
  const length = Number(meta.seconds) > 0 ? clock(Number(meta.seconds)) : "";
  const game = meta.game || "";
  if (length && game) return `A ${length} clip from ${game}`;
  if (length) return `A ${length} clip`;
  return game ? `A clip from ${game}` : "A clip";
}

const STYLE = `html,body{margin:0;height:100%;background:#141517;color:#e8e8ea;font:15px/1.4 system-ui,-apple-system,"Segoe UI",sans-serif}
body{display:flex;flex-direction:column}
video{flex:1;min-height:0;width:100%;background:#000}
.bar{display:flex;gap:16px;align-items:baseline;justify-content:space-between;padding:12px 18px;flex-wrap:wrap}
h1{font-size:15px;font-weight:600;margin:0}
.bar span{color:#8b8d93;font-size:13px}
a{color:#e8e8ea}
.gone{margin:auto;text-align:center;padding:24px}.gone h1{font-size:20px;margin-bottom:8px}.gone p{color:#8b8d93;margin:0}`;

// The page has no script at all, so the policy allows none: only its own style, video and image.
let policy;
async function contentPolicy() {
  if (!policy) {
    const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(STYLE));
    const hash = btoa(String.fromCharCode(...new Uint8Array(digest)));
    policy = `default-src 'none'; style-src 'sha256-${hash}'; media-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`;
  }
  return policy;
}

async function page(env, clip, status) {
  const title = clip ? clip.meta.name.replace(/\.(DVR\.)?mp4$/i, "") || "Clip" : status === 410 ? "This clip has expired" : "Clip not found";
  let head = "";
  let body;
  if (clip) {
    const src = `${clip.origin}/${clip.id}.mp4`;
    const poster = clip.hasThumb ? `${clip.origin}/${clip.id}.jpg` : "";
    const { width = "0", height = "0" } = clip.meta;
    const description = describe(clip.meta);
    // og:video is what Discord and iMessage embed. X only plays a twitter:player that is an HTML
    // page made for it, so it gets the still image card instead of a player that breaks.
    head = `<meta name="description" content="${esc(description)}">
<meta property="og:type" content="video.other">
<meta property="og:title" content="${esc(title)}">
<meta property="og:description" content="${esc(description)}">
<meta property="og:site_name" content="tape">
<meta property="og:url" content="${clip.origin}/${clip.id}">
<meta property="og:video" content="${src}">
<meta property="og:video:secure_url" content="${src}">
<meta property="og:video:type" content="video/mp4">
${width !== "0" ? `<meta property="og:video:width" content="${esc(width)}">\n<meta property="og:video:height" content="${esc(height)}">` : ""}
${poster ? `<meta property="og:image" content="${poster}">` : ""}
<meta name="twitter:card" content="${poster ? "summary_large_image" : "summary"}">
<meta name="twitter:title" content="${esc(title)}">
<meta name="twitter:description" content="${esc(description)}">
${poster ? `<meta name="twitter:image" content="${poster}">` : ""}`;
    body = `<video src="${src}" ${poster ? `poster="${poster}"` : ""} controls autoplay playsinline preload="metadata"></video>
<div class="bar"><h1>${esc(title)}</h1><span>Link expires in ${left(Number(clip.meta.expires) - Date.now())} · <a href="${src}" download>Download</a></span></div>`;
  } else {
    body = `<div class="gone"><h1>${esc(title)}</h1><p>${status === 410 ? "Shared links stop working after a while. Ask for a new one." : "Check the link and try again."}</p></div>`;
  }
  const html = `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(title)}</title><meta name="robots" content="noindex">
${head}
<style>${STYLE}</style></head><body>${body}</body></html>`;
  const headers = { "content-type": "text/html; charset=utf-8", "cache-control": "no-store", "x-content-type-options": "nosniff", "content-security-policy": await contentPolicy() };
  return new Response(html, { status, headers });
}
