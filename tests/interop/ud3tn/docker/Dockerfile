# ud3tn Dockerfile for Interop Testing
# uD3TN - Micro Delay-Tolerant Networking Implementation
#
# Builds ud3tn with Python tools for interoperability testing via MTCP.
#
# Build:
#   docker build -t ud3tn-interop .
#
# Build with specific version:
#   docker build -t ud3tn-interop --build-arg UD3TN_REF=v0.14.0 .
#
# Run:
#   docker run -it --rm --network host \
#       ud3tn-interop \
#       -e ipn:2.0 -c "mtcp:0.0.0.0,4557" -b 7 -A 0.0.0.0 -P 4243 -R

FROM python:3.13-slim@sha256:2b7445fb71ca9cb15e9aab053fe8cb3162796f8e1d92ada12a49c766a811bc1e AS builder

ARG UD3TN_REF=v0.15.0
ARG CORES=4

ENV DEBIAN_FRONTEND=noninteractive

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    ca-certificates \
    build-essential \
    libsqlite3-dev \
    libjansson-dev \
    && rm -rf /var/lib/apt/lists/*

# Clone ud3tn from GitLab
WORKDIR /build
RUN git clone --recursive https://gitlab.com/d3tn/ud3tn.git . && \
    git checkout ${UD3TN_REF} && \
    git submodule update --init --recursive && \
    { d=$(git describe --tags --always --dirty 2>/dev/null || git rev-parse HEAD); \
      m=$(grep -hm1 -E '^[[:space:]]*version[[:space:]]*=[[:space:]]*"[0-9]' Cargo.toml 2>/dev/null | sed -E 's/.*"([^"]+)".*/\1/'); \
      [ -z "$m" ] && m=$(grep -rhim1 'project[[:space:]]*(' CMakeLists.txt 2>/dev/null | grep -oE 'VERSION[[:space:]]+[0-9][0-9.]*' | grep -oE '[0-9][0-9.]*' | head -1); \
      [ -z "$m" ] && m=$(sed -n 's/.*AC_INIT(\[[^]]*\],[[:space:]]*\[\([^]]*\)\].*/\1/p' configure.ac 2>/dev/null | head -1); \
      [ -z "$m" ] && [ -f VERSION ] && m=$(head -1 VERSION); \
      printf '%s%s\n' "$d" "${m:+ (declared: $m)}"; } > /interop-version

# Build ud3tn
RUN ARCH=x86-64 make posix -j${CORES}

# Install Python packages
RUN pip install --no-cache-dir --prefix=/install/python \
    ./pyd3tn \
    ./python-ud3tn-utils

# Runtime stage
FROM python:3.13-slim@sha256:2b7445fb71ca9cb15e9aab053fe8cb3162796f8e1d92ada12a49c766a811bc1e

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    libsqlite3-0 \
    libjansson4 \
    && rm -rf /var/lib/apt/lists/*

# Copy ud3tn binary
COPY --from=builder /build/build/posix/ud3tn /usr/local/bin/ud3tn
COPY --from=builder /interop-version /interop-version

# Copy Python packages
COPY --from=builder /install/python/ /usr/local/

# Copy our start script
COPY start_ud3tn /usr/local/bin/start_ud3tn
RUN chmod +x /usr/local/bin/start_ud3tn /usr/local/bin/ud3tn

EXPOSE 4557/tcp 4242/tcp 4243/tcp

ENTRYPOINT ["/usr/local/bin/start_ud3tn"]
