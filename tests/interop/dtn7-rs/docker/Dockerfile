FROM docker.io/rust:1.86 as builder

# Optional: specify a git ref (tag, branch, or commit hash)
# Build with: docker build --build-arg DTN7_REF=<ref> ...
#
# Pinned to the commit carrying the XFER_ACK flag-reflection fix (dtn7-rs PR #78,
# "Reflect flags in the ACK"). RFC 9174 §5.2.2 requires an XFER_ACK to reflect the
# START/END flags of the XFER_SEGMENT it acknowledges; without #78 dtn7-rs sends
# flags=0, which Hardy correctly rejects, tearing down the TCPCLv4 session. The fix
# is merged upstream but not yet in any tagged release (latest is v0.21.0, which
# predates it), so we pin the commit. `git describe` resolves this to v0.21.0-20-g7e2ae44.
ARG DTN7_REF=7e2ae44
ARG DTN7_PLUS_REF=010202e56

ENV DEBIAN_FRONTEND=noninteractive

WORKDIR /root/

# Install git and clone dtn7-rs
RUN apt-get update && apt-get install -y --no-install-recommends apt-utils git && rm -rf /var/lib/apt/lists/*

RUN git clone https://github.com/dtn7/dtn7-rs.git dtn7 && \
    cd dtn7 && \
    git checkout ${DTN7_REF} && \
    { d=$(git describe --tags --always --dirty 2>/dev/null || git rev-parse HEAD); \
      m=$(grep -hm1 -E '^[[:space:]]*version[[:space:]]*=[[:space:]]*"[0-9]' Cargo.toml 2>/dev/null | sed -E 's/.*"([^"]+)".*/\1/'); \
      [ -z "$m" ] && m=$(grep -rhim1 'project[[:space:]]*(' CMakeLists.txt 2>/dev/null | grep -oE 'VERSION[[:space:]]+[0-9][0-9.]*' | grep -oE '[0-9][0-9.]*' | head -1); \
      [ -z "$m" ] && m=$(sed -n 's/.*AC_INIT(\[[^]]*\],[[:space:]]*\[\([^]]*\)\].*/\1/p' configure.ac 2>/dev/null | head -1); \
      [ -z "$m" ] && [ -f VERSION ] && m=$(head -1 VERSION); \
      printf '%s%s\n' "$d" "${m:+ (declared: $m)}"; } > /interop-version

RUN cd dtn7 && \
    cargo install --locked --bins --examples --root /usr/local --path examples && \
    cargo install --locked --bins --examples --root /usr/local --path core/dtn7

RUN cargo install --locked --bins --examples --root /usr/local dtn7-plus \
    --git https://github.com/dtn7/dtn7-plus-rs --rev ${DTN7_PLUS_REF}

FROM docker.io/debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends apt-utils net-tools iproute2 iputils-ping tcpdump wget curl && rm -rf /var/lib/apt/lists/*

WORKDIR /

COPY --from=builder /usr/local/bin/* /usr/local/bin/
COPY --from=builder /interop-version /interop-version

# Copy start_dtnd wrapper script
COPY start_dtnd /usr/local/bin/start_dtnd
RUN chmod +x /usr/local/bin/start_dtnd

ENV PATH="${PATH}:/usr/local/bin:/usr/local/sbin"

HEALTHCHECK --interval=60s --timeout=10s --start-period=15s --retries=3 \
  CMD curl --fail http://localhost:3000 || exit 1

ENTRYPOINT [ "start_dtnd" ]
