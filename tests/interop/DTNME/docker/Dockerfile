# DTNME Dockerfile for Interop Testing
# NASA DTN Marshal Enterprise Implementation
#
# Builds DTNME from GitHub for interoperability testing
#
# Build:
#   docker build -t dtnme-interop .
#
# Build with specific version:
#   docker build -t dtnme-interop --build-arg DTNME_REF=v1.0.0 .
#
# Run:
#   docker run -it --rm --network host -e NODE_ID=1 dtnme-interop

FROM debian:buster AS builder

# Optional: specify a git ref (tag, branch, or commit hash)
ARG DTNME_REF=master

ENV DEBIAN_FRONTEND=noninteractive

# Buster is archived - update sources to use archive.debian.org
RUN sed -i 's/deb.debian.org/archive.debian.org/g' /etc/apt/sources.list && \
    sed -i 's/security.debian.org/archive.debian.org/g' /etc/apt/sources.list && \
    sed -i '/buster-updates/d' /etc/apt/sources.list

# Install build dependencies
# Debian buster has Boost 1.67 which has Beast and compatible Asio API
RUN apt-get update && apt-get install -y --no-install-recommends \
    apt-utils \
    git \
    ca-certificates \
    build-essential \
    g++ \
    automake \
    autoconf \
    autotools-dev \
    libtool \
    libdb-dev \
    libboost-all-dev \
    libssl-dev \
    libtirpc-dev \
    tcl \
    tcl-dev \
    tk \
    tk-dev \
    && rm -rf /var/lib/apt/lists/*

# Create gmake symlink (Debian uses 'make' not 'gmake')
RUN ln -s /usr/bin/make /usr/bin/gmake

# Clone DTNME from GitHub
WORKDIR /build
RUN git clone https://github.com/nasa/DTNME.git . && \
    git checkout ${DTNME_REF}

# Build DTNME using the provided build script
RUN sh step_1_make_gcc_dtnme.sh && \
    mkdir -p /dtn && \
    sh step_2_install_dtnme.sh /dtn 1

# Runtime stage
FROM debian:buster-slim

ENV DEBIAN_FRONTEND=noninteractive

# Buster is archived - update sources to use archive.debian.org
RUN sed -i 's/deb.debian.org/archive.debian.org/g' /etc/apt/sources.list && \
    sed -i 's/security.debian.org/archive.debian.org/g' /etc/apt/sources.list && \
    sed -i '/buster-updates/d' /etc/apt/sources.list

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    apt-utils \
    libdb5.3 \
    libboost-system1.67.0 \
    libboost-thread1.67.0 \
    libboost-filesystem1.67.0 \
    libboost-program-options1.67.0 \
    libssl1.1 \
    libtcl8.6 \
    libtk8.6 \
    libstdc++6 \
    libtirpc-common \
    && rm -rf /var/lib/apt/lists/*

# Create dtn user and directories
RUN useradd -m -s /bin/bash dtn && \
    mkdir -p /dtn/bin /dtn/conf /dtn/data

# Copy installed files from builder (step_2_install_dtnme.sh puts them in /dtn)
COPY --from=builder /dtn/bin/ /dtn/bin/
COPY --from=builder /dtn/dtnme_daemon.cfg /dtn/conf/dtnme_daemon.cfg

# Copy our start script
COPY start_dtnme /dtn/bin/start_dtnme

RUN chmod +x /dtn/bin/*

# Set ownership
RUN chown -R dtn:dtn /dtn

# Environment variables
ENV NODE_ID=1
ENV TCPCL_PORT=4556
ENV REMOTE_HOST=""
ENV REMOTE_NODE=2
ENV PATH="/dtn/bin:${PATH}"

# Expose standard DTN ports
# 4556 - TCP/UDP Convergence Layer (standard)
# 4556 - Secure TCP Convergence Layer
# 1113 - LTP/UDP
# 5010 - DTN API
# 5050 - Console
EXPOSE 4556/tcp 4556/udp 4556/tcp 1113/udp 5010/tcp 5050/tcp

WORKDIR /dtn
USER dtn

ENTRYPOINT ["/dtn/bin/start_dtnme"]
