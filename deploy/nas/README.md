# NAS deployment

This Compose profile preserves the container identity and WeChat state across
container recreation while limiting background resource usage. It is intended
for a NAS where OpenClaw already uses the external `openclaw_default` network.

## Prepare

Build the image from this repository on an amd64 host, or load an equivalent
image into the NAS Docker daemon:

```bash
pnpm build:image:amd64
```

Create the private deployment files and authentication token:

```bash
cd deploy/nas
cp .env.example .env
mkdir -p data wechat-home secrets
openssl rand -hex 32 > secrets/auth-token
chmod 600 secrets/auth-token
```

Edit `.env` before the first start. Choose a locally administered MAC address
that is unique on `openclaw_default`, then keep both
`AGENT_WECHAT_HOSTNAME` and `AGENT_WECHAT_MAC_ADDRESS` unchanged. You may replace
the relative storage paths with NAS-specific absolute paths in `.env`; do not
commit that file.

The external network must already exist. Create it only if OpenClaw has not
created it:

```bash
docker network inspect openclaw_default >/dev/null 2>&1 || \
  docker network create openclaw_default
```

## Start or recreate

```bash
docker compose --env-file .env up -d
docker compose ps
docker compose logs --tail=100 agent-wechat
```

Recreating the service is safe when the hostname, MAC address, token, `/data`,
and `/home/wechat` mounts stay the same. Changing or deleting those values can
make WeChat treat the container as a new Linux client and require another QR
login.

The API and noVNC endpoint bind to `127.0.0.1:6174` by default. Containers on
`openclaw_default` can reach the service at `http://agent-wechat:6174`; remote
browser access should go through an authenticated reverse proxy or an SSH
tunnel rather than exposing the port directly.

## Resource and login safeguards

- CPU is capped at 0.75 core and the container is limited to 384 processes.
- Process health checks run every 5 seconds; the heavier UI scan runs every 30
  seconds.
- Only one `/api/ws/login` flow can run at a time. A concurrent connection gets
  an `already in progress` error instead of entering the GUI execution queue.
- The token, persistent state, NAS paths, and the actual device MAC remain local
  and are not part of this repository.
