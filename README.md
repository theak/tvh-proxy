# tvh-proxy

A sleek, password-protected live TV front end for [tvheadend](https://tvheadend.org) that you can safely put on the internet and watch from an iPhone.

- **Full-screen viewer** with now/next info, ↑/↓ buttons or arrow keys to change channel, `G` for the guide.
- **Guide** built from tvheadend's EPG, with keyboard and touch navigation.
- **Plays on iPhone.** tvheadend's raw broadcast (MPEG-2 video, AC-3 audio) won't play in Safari, so the proxy re-encodes the channel you pick to H.264/AAC with ffmpeg and serves it as 1-second HLS segments. Typical start-up is ~3 seconds.
- **Jump-to keywords.** e.g. `nfl, olympics`: when you open the app, the first keyword that matches something on air tunes straight to it; otherwise you land in the guide.
- **Shared viewing.** `max viewers` caps how many different channels can play at once (usually your tuner count). Anyone who opens the app while every slot is busy joins the current channel, can't change it, and sees a banner asking them to close the other viewer. When that viewer closes, control passes to the next one.
- **One login**, stored as an argon2 hash; sessions survive restarts; logins are rate-limited.

## Run it

```sh
docker compose up -d        # see docker-compose.yml
docker logs tvh-proxy       # shows the one-time setup code
```

Open `http://<host>:9982`, enter the setup code, then:

0. tvheadend's address (e.g. `http://192.168.0.149:9981`), plus its login if anonymous access is off
1. your username + password: **write them down**, they're never shown again
2. jump-to keywords (optional)
3. max viewers (default 1)

To change these later, use the gear icon in the viewer. To start over, delete `config.json` from the data volume and restart; a new setup code is printed.

tvheadend must let this host stream and read the EPG, either anonymously (an access entry for its IP with *streaming* and *web interface* rights) or with the tvheadend login you enter during setup.

## Exposing it to the internet

The container speaks plain HTTP. Put HTTPS in front of it: Caddy, nginx, or a Cloudflare Tunnel. Then:

- set `COOKIE_SECURE=true` (it's also set automatically when the proxy sends `X-Forwarded-Proto: https`)
- set `TRUST_PROXY=true` **only** if that proxy sets `X-Forwarded-For`, so login throttling sees real client IPs

Setup is gated by the code from the container log, so a stranger who finds the URL before you finish can't claim it.

On an iPhone, open the site in Safari → Share → **Add to Home Screen** to get a full-screen app.

## Tuning

| Variable | Default | |
|---|---|---|
| `PORT` | `9982` | HTTP port |
| `DATA_DIR` | `/data` | settings + sessions |
| `HWACCEL` | *(unset)* | `vaapi` to encode on an Intel iGPU (pass `/dev/dri` into the container) |
| `VAAPI_DEVICE` | `/dev/dri/renderD128` | |
| `VIDEO_HEIGHT` | `720` | output height (never upscales) |
| `VIDEO_BITRATE_KBPS` | `3500` | video bitrate; also roughly the upload each viewer uses |
| `FFMPEG_PATH` | `ffmpeg` | |

Software encoding (libx264 `veryfast`, `zerolatency`) of one 720p30 stream needs roughly two modern CPU cores. Channel changes are debounced in the browser (~0.7 s) so flicking through channels doesn't hammer the tuner.

## Develop

```sh
cargo test && npm test
DATA_DIR=./data cargo run      # http://localhost:9982
```

Frontend assets (`templates/`, `static/`) are baked into the binary, so rebuild after editing them. `static/js/hls.min.js` is a vendored copy of [hls.js](https://github.com/video-dev/hls.js) 1.7.3, used by browsers without native HLS. Geist fonts are under the SIL Open Font License.

Pushes to `main` run the tests and publish a multi-arch image to Docker Hub as `akshaykannan/tvh-proxy:latest`.
