FROM docker.io/library/rust:1.90-bookworm AS build
RUN apt-get update && apt-get install --no-install-recommends -y python3 pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /source
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY workers ./workers
COPY .iii-version ./
RUN cargo build --workspace --release --locked
COPY config.yaml worker-compose.yaml .env.example ./
COPY config ./config
COPY agents ./agents
COPY hands ./hands
COPY identity ./identity
COPY integrations ./integrations
COPY plugin ./plugin
COPY workflows ./workflows
COPY scripts/stage-runtime.py /tmp/stage-runtime.py
RUN python3 /tmp/stage-runtime.py /source /bundle

FROM docker.io/library/debian:trixie-slim
RUN apt-get update && apt-get install --no-install-recommends -y ca-certificates curl file python3 python3-venv tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 1000 agentos
COPY --from=build /bundle/bin/ /usr/local/bin/
COPY --from=build /bundle/runtime/ /opt/agentos/runtime/
# Private checkout modes must not hide immutable templates from the runtime user.
RUN chmod -R a+rX /opt/agentos/runtime
COPY scripts/install-iii.sh /opt/agentos/scripts/install-iii.sh
COPY .iii-version /opt/agentos/.iii-version
RUN III_INSTALL_DIR=/usr/local/bin bash /opt/agentos/scripts/install-iii.sh \
    && python3 -m venv /opt/agentos/python \
    && /opt/agentos/python/bin/pip install --no-cache-dir "iii-sdk==$(cat /opt/agentos/.iii-version)"
COPY --chmod=0644 scripts/container-entrypoint.py /opt/agentos/container-entrypoint.py
ENV PATH=/opt/agentos/python/bin:$PATH \
    HOME=/home/agentos/.agentos \
    AGENTOS_HOME=/home/agentos/.agentos \
    AGENTOS_CONTAINER_RUNTIME=1
USER agentos
WORKDIR /home/agentos
ENTRYPOINT ["/usr/bin/tini", "--", "python3", "/opt/agentos/container-entrypoint.py"]
