# readeckobo

readeckobo is a Readeck-to-Kobo proxy. It serves Kobo's Instapaper API from
Readeck and proxies the Kobo Store API to either Kobo or a self-hosted
Kobo-compatible backend such as BookLore, Grimmory, or BookOrbit.

All Kobo traffic uses one public endpoint. The reverse proxy forwards requests
to readeckobo, and readeckobo decides where each request goes.

```text
Kobo ──► readeckobo
          ├─ /storeapi/v1/initialization
          │     fetch from the backend, rewrite the Instapaper URL,
          │     preserve the backend's library URLs
          ├─ /storeapi/*
          │     stream to the backend
          ├─ /instapaper/api/kobo/*
          │     Readeck article sync, download, and actions
          ├─ /convert-image
          │     fetch and re-encode images as JPEG
          └─ /healthz
```

The Store backend is one of:

- **Kobo** (default): `https://storeapi.kobo.com`
- **Custom**: BookLore, Grimmory, BookOrbit, or another Kobo-compatible
  backend, configured with `kobo.custom_backend_url` (including its token path).

## Quick start

```sh
cp config.yaml.example config.yaml
# edit config.yaml
docker compose up -d --build
```

The service listens on `127.0.0.1:8080`.

With rootless Podman, bind mounts need the host user's UID and an SELinux
label:

```sh
podman run -d --name readeckobo \
  --userns=keep-id \
  -v "$PWD/config.yaml:/app/config.yaml:ro,Z" \
  -p 127.0.0.1:8080:8080 \
  readeckobo:local
```

The `readeckobo.build` and `readeckobo.container` Quadlet files provide the
same build, mount, and port setup for a systemd user service.

## Reverse proxy

Caddy only needs to forward requests:

```caddyfile
readeckobo.example.com {
	request_body {
		max_size 128MiB
	}
	reverse_proxy 127.0.0.1:8080
}
```

## Kobo setup

Generate a device token:

```sh
docker compose run --rm readeckobo token <KOBO-SERIAL> \
  --public-url https://readeckobo.example.com
```

If the Kobo is mounted, the serial is read from `.kobo/version`:

```sh
docker compose run --rm readeckobo token \
  --kobo /media/$USER/KOBO \
  --public-url https://readeckobo.example.com
```

The command prints the plaintext token for `config.yaml`, the encrypted token
for the Kobo, and the matching `[OneStoreServices]` snippet. Kobo stores an
encrypted Instapaper access token in `Kobo eReader.conf` and decrypts it before
sending requests.

Add the token to `config.yaml`:

```yaml
users:
  - token: "<PLAINTEXT-TOKEN>"
    readeck_access_token: "<READECK-API-TOKEN>"
```

In `.kobo/Kobo/Kobo eReader.conf`:

```ini
[OneStoreServices]
api_endpoint=https://readeckobo.example.com/storeapi
instapaper_env_url=https://readeckobo.example.com/instapaper/api/kobo

[Instapaper]
AccessToken=@ByteArray(<ENCRYPTED-TOKEN>)
```

`instapaper_env_url` includes `/api/kobo`; Kobo appends `/get`, `/download`,
and `/send` to it.

### Migrating an existing installation

The device token is independent of the server implementation. Reuse the
plaintext `users[].token` from the old `config.yaml` and keep the existing
`[Instapaper] AccessToken=@ByteArray(...)` in `Kobo eReader.conf`. Only
`api_endpoint` and `instapaper_env_url` change.

To verify that the encrypted value matches, encrypt the old plaintext token
with the device serial:

```sh
readeckobo token <KOBO-SERIAL> --plaintext <OLD-PLAINTEXT-TOKEN>
```

The printed `AccessToken` should match the one already in `Kobo eReader.conf`.

## Configuration

| Key | Description | Default |
| :--- | :--- | :--- |
| `server.port` | Listen port. | `8080` |
| `log_level` | `error`, `warn`, `info`, `debug` (`RUST_LOG` overrides). | `info` |
| `public_url` | Public HTTPS origin used in initialization rewriting. | derived from forwarding headers |
| `readeck.host` | Readeck base URL. | required |
| `kobo.backend` | `kobo` or `custom`. | `kobo` |
| `kobo.custom_backend_url` | Kobo-compatible backend base URL, including any token path. | — |
| `kobo.store_api_url` | Legacy alias for `custom_backend_url`. | — |
| `kobo.remove_archived_from_kobo` | Send archived items as status `2` (remove from device) instead of `1`. | `false` |
| `users[].token` | Plaintext device token. | required |
| `users[].readeck_access_token` | Readeck API token. | required |

Set `public_url` when the ingress cannot supply `X-Forwarded-Proto` and
`X-Forwarded-Host`. It also keeps the rewritten Instapaper URL canonical.

The Readeck API token needs `bookmarks:write` for archive, favorite, and delete
actions. Read-only tokens make those actions fail with `403 Forbidden`.
readeckobo checks token permissions at startup and logs a warning when write
permission is missing. Sync and article downloads need `bookmarks:read`.

## Kobo action mapping

| Kobo action | Readeck request |
| :--- | :--- |
| `archive` | `PATCH /api/bookmarks/{id}` `{"is_archived": true}` |
| `readd` | `PATCH /api/bookmarks/{id}` `{"is_archived": false}` |
| `favorite` | `PATCH /api/bookmarks/{id}` `{"is_marked": true}` |
| `unfavorite` | `PATCH /api/bookmarks/{id}` `{"is_marked": false}` |
| `delete` | `DELETE /api/bookmarks/{id}` |
| `add` | `POST /api/bookmarks` `{"url": "..."}` |
| `opened_item`, `left_item` | no-op |

The `send` response uses the Pocket/Instapaper shape:
`{"status": 1, "action_results": [true], "action_errors": [null]}`. `status`
is an integer, and failures are reported per action.

Delete uses Readeck's `DELETE` endpoint. A `PATCH` with `{"is_deleted": true}`
schedules an asynchronous deletion task instead.

## Behavior

- **Streaming proxy.** Store requests other than initialization are streamed,
  so library syncs and EPUB downloads are not held in memory.
- **Initialization.** The initialization response is fetched with
  `Accept-Encoding: identity`, capped at 8 MiB, and only the Instapaper URL is
  rewritten. Library, image, and download URLs are preserved.
- **Images.** Article `<img>` tags become `<!--IMG_n-->` comments, and non-JPEG
  image URLs are rewritten to `/convert-image?url=...`.
- **Download forms.** Form-encoded download requests preserve semicolons inside
  URL values.
- **Logging.** Request paths are logged without query strings; headers and
  bodies are not logged.
- **Shutdown.** SIGINT and SIGTERM stop the server cleanly.
- **Health.** `GET /healthz` returns `ok`.

## Development

```sh
cargo run -- --config config.yaml
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

The integration tests cover backend routing, initialization rewriting, Kobo
sync (full and incremental), action mapping, image rewriting, form parsing, and
JPEG conversion.

## Security

`/convert-image` fetches caller-provided URLs. Keep the service behind a
trusted network, and block that endpoint at the edge if it is not needed. Image
URLs must use HTTP or HTTPS, but the service does not perform SSRF filtering.

`config.yaml` contains credentials and is gitignored. Keep it out of images and
logs.
