# tvh-proxy

A sleek, password-protected live TV web frontend for [tvheadend](https://tvheadend.org) that you can safely put on the internet (behind username / password auth) and watch from any mobile web or desktop device:

<p align="center">
 <img width="796" height="443" alt="tvh-proxy" src="https://github.com/user-attachments/assets/e37aec33-41b9-4c5c-826c-2efd5a1fac68" />
 <img width="796" height="442" alt="tvh-guide" src="https://github.com/user-attachments/assets/e054afeb-5da1-455b-882a-90722bb61d5f" />
</p>

- **Full-screen viewer** with now/next info, ↑/↓ buttons or arrow keys to change channel, `G` for the guide, and ✕ (or `S` / `Esc`) to stop and free the tuner.
- **Guide** built from tvheadend's EPG, with keyboard and touch navigation.
- **Plays on iPhone.** tvheadend's raw broadcast (MPEG-2 video, AC-3 audio) won't play in Safari, so the proxy re-encodes the channel you pick to H.264/AAC with ffmpeg and serves it as 1-second HLS segments, in HD (720p) and SD (360p, ~0.9 Mbps). Each device switches between them on its own as its connection changes, so a phone on weak cellular drops to SD without affecting anyone else. Typical start-up is ~3 seconds.

## Run it
```sh
docker run -d \
  --name tvh-proxy \
  --restart unless-stopped \
  -p 9982:9982 \
  -v tvh-proxy-data:/data \
  akshaykannan/tvh-proxy:latest
```

Or using docker compose with [docker-compose.yml](docker-compose.yml)
```sh
docker compose up -d        # see docker-compose.yml
docker logs tvh-proxy       # shows the one-time setup code
```

Open `http://<host>:9982`, enter the setup code, then:

0. tvheadend's address (e.g. `http://192.168.0.149:9981`), plus its login if anonymous access is off
1. your username + password: **write them down**, they're never shown again
2. jump-to keywords (optional)

To change these later, the admin uses the gear icon (in the viewer or the guide). Lost the admin password? Run `docker exec tvh-proxy tvh-proxy reset-admin`, which prints a new one without touching other accounts, then restart the container. To start over completely, delete `config.json` from the data volume and restart; a new setup code is printed.

tvheadend must let this host stream and read the EPG, either anonymously (an access entry for its IP with *streaming* and *web interface* rights) or with the tvheadend login you enter during setup.

## Exposing it to the internet

The container speaks plain HTTP. Put HTTPS in front of it: Caddy, nginx, or a Cloudflare Tunnel. Nothing else needs configuring: cookies are marked Secure automatically when the proxy sends `X-Forwarded-Proto: https`, and plain-http access on your LAN (or over Tailscale) keeps working alongside it. Only set `COOKIE_SECURE=true` if *every* visit goes through HTTPS.

Login throttling goes by the connecting address and ignores forwarded headers (they can be forged), so behind a tunnel all remote visitors share one throttle. That makes guessing slower, at the cost that a bot can briefly hold up new remote sign-ins; anyone already signed in is unaffected. Sign-ins last 400 days and renew whenever the app is opened, so once a device is in, it stays in. Changing a password or deleting an account signs it out everywhere.

Every failed sign-in is written to the container log (`docker logs tvh-proxy | grep "failed sign-in"`). To be alerted too, set a **failed sign-in webhook** in settings, such as a Home Assistant webhook.

## Tuning

| Variable | Default | |
|---|---|---|
| `PORT` | `9982` | HTTP port |
| `DATA_DIR` | `/data` | settings + sessions |
| `COOKIE_SECURE` | *(unset)* | `true` to always mark cookies Secure; only if every visit is over HTTPS |
| `HWACCEL` | *(unset)* | `vaapi` to encode on an Intel iGPU (pass `/dev/dri` into the container) |
| `VAAPI_DEVICE` | `/dev/dri/renderD128` | |
| `VIDEO_HEIGHT` | `720` | HD rendition height (never upscales); SD is always 360p |
| `VIDEO_BITRATE_KBPS` | `3500` | HD video bitrate; also roughly the upload each HD viewer uses (SD is 800) |
| `FFMPEG_PATH` | `ffmpeg` | |

Software encoding (libx264 `veryfast`, `zerolatency`) of one stream (720p30 plus the 360p copy) needs roughly two to three modern CPU cores. Channel changes are debounced in the browser (~0.7 s) so flicking through channels doesn't hammer the tuner.

## Develop

```sh
cargo test && npm test
DATA_DIR=./data cargo run      # http://localhost:9982
```

Frontend assets (`templates/`, `static/`) are baked into the binary, so rebuild after editing them. `static/js/hls.min.js` is a vendored copy of [hls.js](https://github.com/video-dev/hls.js) 1.7.3, used by browsers without native HLS. Geist fonts are under the SIL Open Font License.

Pushes to `main` run the tests and publish a multi-arch image to Docker Hub as `akshaykannan/tvh-proxy:latest`.
