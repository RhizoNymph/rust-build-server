# MinIO on node0 (`rbs-minio`)

The shared kache store is an S3 bucket served by a MinIO container on node0.
It is already running; this file records how it was created so it can be
rebuilt.

## Credentials

All secrets live in `~/.config/rbs/minio.env` (mode 600, never committed):

```sh
MINIO_ROOT_USER=rbs
MINIO_ROOT_PASSWORD=<long random string>
KACHE_S3_ACCESS_KEY=rbs
KACHE_S3_SECRET_KEY=<same as MINIO_ROOT_PASSWORD, or a dedicated service key>
KACHE_S3_ENDPOINT=http://<minio-host>:9100   # LAN address of the box running MinIO
KACHE_S3_BUCKET=kache
```

`rbs setup` reads only the `KACHE_S3_*` lines, writes the `[rbs]` profile into
`~/.aws/credentials`, and generates `~/.config/kache/config.toml`. The same
file must exist on the laptop (copy it with `scp`), because kache on the
laptop talks to the same bucket.

## Container

`docker` on node0 is the snap package, which cannot read dot-directories under
`$HOME`. The data directory is therefore `~/rbs-data/minio`, not
`~/.local/share/...`, and the env file is passed by value (`-e VAR`, picked
up from the shell) rather than `--env-file ~/.config/rbs/minio.env`.

```sh
set -a; . ~/.config/rbs/minio.env; set +a
mkdir -p ~/rbs-data/minio
docker run -d --name rbs-minio --restart unless-stopped \
  -p 9100:9000 -p 9101:9001 \
  -e MINIO_ROOT_USER -e MINIO_ROOT_PASSWORD \
  -v ~/rbs-data/minio:/data \
  minio/minio:latest server /data --console-address ":9001"
```

- S3 API: `http://<minio-host>:9100` (what kache uses).
- Console: `http://<minio-host>:9101`.

## Bucket

Created once with the `mc` client:

```sh
docker run --rm -it --network host \
  -e MC_HOST_rbs="http://$MINIO_ROOT_USER:$MINIO_ROOT_PASSWORD@127.0.0.1:9100" \
  minio/mc mb rbs/kache
```

kache stores objects under the `artifacts/` prefix inside the bucket (see the
`prefix` key in the generated kache config).

## Operations

```sh
docker logs -f rbs-minio            # server log
docker restart rbs-minio            # after changing minio.env
du -sh ~/rbs-data/minio             # on-disk size
kache stats                         # from either host: must show `Remote:     s3://kache/artifacts`
```

To wipe the cache, delete the bucket contents (`mc rm -r --force rbs/kache`),
not the container.
