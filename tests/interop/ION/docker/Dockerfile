# ION Dockerfile for Interop Testing
# NASA Interplanetary Overlay Network (ION) DTN Implementation
#
# Builds ION from GitHub for interoperability testing via STCP.
#
# Build:
#   docker build -t ion-interop .
#
# Build with specific version:
#   docker build -t ion-interop --build-arg ION_REF=ion-open-source-4.1.4 .
#
# Build with more cores:
#   docker build -t ion-interop --build-arg CORES=8 .
#
# Run:
#   docker run -it --rm --network host --ipc=host \
#       -e ION_NODE_NUM=2 -e STCP_PORT=4557 \
#       ion-interop

FROM debian:trixie-slim AS builder

ARG ION_REF=ion-open-source-4.1.4
ARG CORES=4

ENV DEBIAN_FRONTEND=noninteractive

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    ca-certificates \
    build-essential \
    automake \
    autoconf \
    autoconf-archive \
    libtool \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Clone ION from GitHub
WORKDIR /build
RUN git clone https://github.com/nasa-jpl/ION-DTN.git . && \
    git checkout ${ION_REF} && \
    { d=$(git describe --tags --always --dirty 2>/dev/null || git rev-parse HEAD); \
      m=$(grep -hm1 -E '^[[:space:]]*version[[:space:]]*=[[:space:]]*"[0-9]' Cargo.toml 2>/dev/null | sed -E 's/.*"([^"]+)".*/\1/'); \
      [ -z "$m" ] && m=$(grep -rhim1 'project[[:space:]]*(' CMakeLists.txt 2>/dev/null | grep -oE 'VERSION[[:space:]]+[0-9][0-9.]*' | grep -oE '[0-9][0-9.]*' | head -1); \
      [ -z "$m" ] && m=$(sed -n 's/.*AC_INIT(\[[^]]*\],[[:space:]]*\[\([^]]*\)\].*/\1/p' configure.ac 2>/dev/null | head -1); \
      [ -z "$m" ] && [ -f VERSION ] && m=$(head -1 VERSION); \
      printf '%s%s\n' "$d" "${m:+ (declared: $m)}"; } > /interop-version

# Build ION using autotools
RUN autoreconf -fi && \
    ./configure --prefix=/usr/local && \
    make -j${CORES} && \
    make DESTDIR=/install install

# Runtime stage
FROM debian:trixie-slim@sha256:28de0877c2189802884ccd20f15ee41c203573bd87bb6b883f5f46362d24c5c2

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*

# Copy installed ION binaries and libraries
COPY --from=builder /install/usr/local/ /usr/local/
COPY --from=builder /interop-version /interop-version

# Refresh shared library cache
RUN ldconfig

# Copy our start script
COPY start_ion /usr/local/bin/start_ion
RUN chmod +x /usr/local/bin/start_ion

# Create ion user and working directory
RUN useradd -m -s /bin/bash ion
RUN mkdir -p /ion && chown ion:ion /ion

# Environment variables (overridden at runtime)
ENV ION_NODE_NUM=2
ENV STCP_PORT=4557
ENV REMOTE_HOST=""
ENV REMOTE_PORT=4558
ENV REMOTE_NODE=1

EXPOSE 4557/tcp

WORKDIR /ion
USER ion

ENTRYPOINT ["/usr/local/bin/start_ion"]
