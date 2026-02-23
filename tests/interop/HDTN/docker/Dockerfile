##############################################
# HDTN Interop Testing Docker Image
#
# Builds HDTN from GitHub for interoperability testing
#
# Build:
#   docker build -t hdtn-interop -f Dockerfile.interop .
#
# Build with specific version:
#   docker build -t hdtn-interop -f Dockerfile.interop --build-arg HDTN_REF=v1.0.0 .
#
# Run:
#   docker run -it --rm --network host -e NODE_ID=10 hdtn-interop
#
##############################################

##############################################
# Builder
##############################################
FROM debian:11-slim AS builder

# Optional: specify a git ref (tag, branch, or commit hash)
ARG HDTN_REF=master
ARG CORES=4

ENV DEBIAN_FRONTEND=noninteractive

# Build dependencies
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        apt-utils \
        git \
        ca-certificates \
        unzip \
        cmake \
        build-essential \
        libboost-dev \
        libboost-all-dev \
        openssl \
        libssl-dev \
        libzmq3-dev && \
    rm -rf /var/lib/apt/lists/*

# Clone HDTN from GitHub
WORKDIR /opt
RUN git clone https://github.com/nasa/HDTN.git hdtn-src && \
    cd hdtn-src && \
    git checkout ${HDTN_REF}

# Build and install HDTN
RUN cd /opt/hdtn-src && \
    mkdir -p build && \
    cd build && \
    cmake \
        -DBoost_USE_STATIC_LIBS=ON \
        -DCMAKE_INSTALL_PREFIX=/opt/hdtn \
        .. && \
    make -j${CORES} && \
    make install

##############################################
# HDTN Interop Testing Container
##############################################
FROM debian:11-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        apt-utils \
        libc6 \
        libstdc++6 \
        libzmq5 \
        # Networking tools for interop testing
        iproute2 \
        iputils-ping \
        net-tools \
        tcpdump \
        netcat-openbsd \
        curl \
        # Useful utilities
        procps \
        vim-tiny \
        less && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /opt/hdtn /opt/hdtn
COPY --from=builder /opt/hdtn-src/config_files /opt/hdtn/config_files
COPY --from=builder /opt/hdtn-src/tests/test_scripts_linux /opt/hdtn/test_scripts

# Copy start_hdtn wrapper script
COPY start_hdtn /usr/local/bin/start_hdtn
RUN chmod +x /usr/local/bin/start_hdtn

ENV PATH="${PATH}:/opt/hdtn/bin:/usr/local/bin"
ENV HDTN_CONFIG_DIR="/opt/hdtn/config_files"

WORKDIR /opt/hdtn

ENTRYPOINT [ "start_hdtn" ]
