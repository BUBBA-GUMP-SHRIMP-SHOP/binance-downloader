# kline-web-provider container

Start the service on port 9001 with the host archive directory and automatic
restarts:

```bash
cd docker
docker compose up -d
```

Compose mounts `/home/jbc/.local/share/binance-archive` at `/app/data` and runs
the service as its owner's UID/GID 1000 so downloads can write to the directory.
The service is available at `http://localhost:9001` and uses `restart: always`.

Build from the workspace root:

```bash
docker build -f docker/Dockerfile -t kline-web-provider .
docker run --rm -p 8000:8000 -v kline-archives:/app/data kline-web-provider
```

The app runs as UID 10001 from `/app`, listens on port 8000, and stores archives
under `/app/data/data/spot/monthly/klines`. `/app/data` is the archive root passed
to `--destination`. A named volume preserves archives between runs. Bind-mounted
directories must be writable by UID 10001.

Arguments after the image name replace the default command. Include the archive
destination when supplying downloader options:

```bash
docker run --rm -p 8000:8000 -v kline-archives:/app/data \
  kline-web-provider --destination /app/data --jobs 8 --quiet
```
