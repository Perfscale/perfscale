FROM rust:1.88-slim-bookworm

# k6 + locust — the two external engines `perfscale run` can shell out to.
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl gnupg ca-certificates python3 python3-pip && \
    curl -fsSL https://dl.k6.io/key.gpg | gpg --dearmor -o /usr/share/keyrings/k6-archive-keyring.gpg && \
    echo "deb [signed-by=/usr/share/keyrings/k6-archive-keyring.gpg] https://dl.k6.io/deb stable main" \
        > /etc/apt/sources.list.d/k6.list && \
    apt-get update && apt-get install -y --no-install-recommends k6 && \
    pip install --no-cache-dir --break-system-packages locust && \
    rm -rf /var/lib/apt/lists/*

# cargo-watch for hot-reload
RUN cargo install cargo-watch --locked --quiet

WORKDIR /app
COPY . .

CMD ["cargo-watch", "-x", "run -p perfscale-cli -- --help", "-w", "crates"]
