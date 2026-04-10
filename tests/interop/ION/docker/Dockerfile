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

FROM debian:bookworm AS builder

ARG ION_REF=integration
ARG CORES=4

ENV DEBIAN_FRONTEND=noninteractive

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    ca-certificates \
    build-essential \
    automake \
    autoconf \
    libtool \
    && rm -rf /var/lib/apt/lists/*

# Clone ION from GitHub
WORKDIR /build
RUN git clone https://github.com/nasa-jpl/ION-DTN.git . && \
    git checkout ${ION_REF}

# Build ION using autotools
RUN autoreconf -fi && \
    ./configure --prefix=/usr/local && \
    make -j${CORES} && \
    make DESTDIR=/install install

# Runtime stage
FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
    libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*

# Copy installed ION binaries and libraries
COPY --from=builder /install/usr/local/ /usr/local/

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
