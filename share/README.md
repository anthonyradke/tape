# Share server

The server behind tape's Copy link button: a Cloudflare Worker with an R2 bucket. tape uploads a 1080p H.264 copy
of a clip, and the Worker serves it at `https://<worker>/<id>` as a page that plays it, with the Open Graph tags that
let Discord, iMessage and other chat apps embed the video. Links stop working after `LINK_DAYS` (7 by default), and
the bucket deletes the files a day later.

It fits in Cloudflare's free plan. R2 includes 10 GB of storage and charges nothing for downloads. The Worker refuses
uploads once the bucket reaches `CAP_PERCENT` (95%) of `FREE_BYTES`, so storage can't go past the free amount. tape
shows how full it is in Settings and in the gallery, and warns at 80% and 95%.

## Setup

R2 has to be enabled on the account first (Cloudflare asks for a payment method even on the free plan). Then, from
this folder, with [Wrangler](https://developers.cloudflare.com/workers/wrangler/) signed in or `CLOUDFLARE_API_TOKEN`
set to a token from the "Edit Cloudflare Workers" template:

```sh
npx wrangler r2 bucket create tape-clips
npx wrangler r2 bucket lifecycle add tape-clips expire-clips c/ --expire-days 8
npx wrangler r2 bucket lifecycle add tape-clips abort-uploads "" --abort-multipart-days 1
npx wrangler deploy
key=$(openssl rand -base64 33 | tr '+/' '-_' | tr -d '=\n')
printf %s "$key" | npx wrangler secret put UPLOAD_KEY
echo "$key"
```

`deploy` prints the Worker's address. In tape, open Settings > Sharing and enter that address as the share server and
the key printed last as the upload key. The key is only held in the shell variable, so it never lands in a file inside
the repository; keep it in a password manager if it's needed again. Keep it private: anyone who has it can upload to
and delete from the bucket. Running `secret put` again with a new key replaces it.

If `LINK_DAYS` changes in `wrangler.toml`, set the `expire-clips` rule to one day more.

## API

Requests under `/api` need `Authorization: Bearer <upload key>`.

| Request | Does |
|---|---|
| `GET /api/usage` | Bytes stored, the free amount and the cap, and every live link. Deletes expired ones on the way |
| `POST /api/clips` | Starts an upload from `{name, bytes, seconds, width, height, source}`. Refused with 507 if it wouldn't fit under the cap |
| `PUT /api/clips/<id>/parts/<n>?upload=<upload id>` | One part of the file, `partBytes` long except the last |
| `POST /api/clips/<id>/complete?upload=<upload id>` | Finishes the upload from `{parts: [{part, etag}]}` |
| `PUT /api/clips/<id>/thumb` | A JPEG preview image for chat apps, optional |
| `DELETE /api/clips/<id>` | Deletes a link and its files |

Uploads go in 20 MiB parts because a Worker on the free plan accepts at most 100 MB per request. Without the key, the
Worker serves only `/<id>`, `/<id>.mp4` and `/<id>.jpg`, and only for clips that haven't expired.
