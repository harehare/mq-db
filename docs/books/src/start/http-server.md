# HTTP Server

`mq-db serve` starts an HTTP server (built on [axum](https://github.com/tokio-rs/axum)) exposing SQL and mq query endpoints over the indexed store.

```bash
mq-db serve --db store.mq-db              # listens on 127.0.0.1:7878
mq-db serve --db store.mq-db --port 8080  # custom port
mq-db serve --db store.mq-db --host 0.0.0.0 --port 8080
```

## Securing the server

`--host 0.0.0.0` exposes the query endpoints beyond localhost. When doing so, secure the server with an API key or Basic auth, and consider TLS and a rate limit:

```bash
mq-db serve --db store.mq-db --host 0.0.0.0 \
  --api-key "$MQ_DB_API_KEY" \
  --rate-limit 20 \
  --timeout 10 \
  --tls-cert cert.pem --tls-key key.pem
```

| Option | Description |
| --- | --- |
| `--timeout <SECS>` | Abort a request and return `408` if it runs longer than this many seconds |
| `--rate-limit <N>` | Max requests per second per client IP; excess requests get `429` |
| `--api-key <KEY>` | Require `Api-Key: <KEY>` or `Authorization: Bearer <KEY>` (env `MQ_DB_API_KEY`) |
| `--basic-auth <USER:PASS>` | Require HTTP Basic auth (env `MQ_DB_BASIC_AUTH`) |
| `--tls-cert` / `--tls-key` | PEM certificate/key pair to serve over HTTPS instead of plain HTTP |

If both `--api-key` and `--basic-auth` are set, either credential grants access. `--tls-cert` and `--tls-key` must be provided together.

## Endpoints

| Method | Path | Body | Description |
| --- | --- | --- | --- |
| `GET` | `/health` | — | `{"status":"ok","documents":<n>}` |
| `POST` | `/sql` | `{"query":"SELECT …"}` | Execute a SQL query, returns JSON rows |
| `POST` | `/mq` | `{"code":".h1"}` | Evaluate an mq expression, returns `{"results":[…]}` |

## Examples

```bash
# Health check
curl http://127.0.0.1:7878/health

# SQL via HTTP
curl -s -X POST http://127.0.0.1:7878/sql \
  -H 'Content-Type: application/json' \
  -d '{"query":"SELECT block_type, count(*) FROM blocks GROUP BY block_type"}'

# mq via HTTP
curl -s -X POST http://127.0.0.1:7878/mq \
  -H 'Content-Type: application/json' \
  -d '{"code":".h1"}'
```
