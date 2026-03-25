##############################################
# NASA cFS BPNode Interop Testing Docker Image
#
# Builds cFS with BPNode using an STCP PSP module for
# interoperability testing with Hardy.
#
# Build (context = NASA-cFS/ directory):
#   docker build -t cfs-interop -f docker/Dockerfile .
#
# Run:
#   docker run -it --rm --network host --privileged cfs-interop
#
##############################################

##############################################
# Builder
##############################################
FROM ubuntu:22.04 AS builder

ARG CFS_REF=main
ARG BPNODE_REF=main
ARG BPLIB_REF=main
ARG CF_REF=main
ARG TINYCBOR_REF=v0.6.0
ARG CORES=4

ENV DEBIAN_FRONTEND=noninteractive

# Build dependencies
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        git \
        ca-certificates \
        cmake \
        build-essential \
        gcc-multilib \
        libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# CFS_APP_PATH tells cFS CMake where to find app/lib sources
ENV CFS_APP_PATH=/build/src

# Clone cFS framework (with submodules: cFE, OSAL, PSP, sample_defs)
RUN git clone --depth 1 --branch ${CFS_REF} --recurse-submodules \
        https://github.com/nasa/cFS.git cfs-bundle

# Clone BPNode (directory name must match module name for cFS CMake discovery)
RUN git clone --depth 1 --branch ${BPNODE_REF} \
        https://github.com/nasa/bp.git src/bpnode

# Clone BPLib
RUN git clone --depth 1 --branch ${BPLIB_REF} \
        https://github.com/nasa/bplib.git src/bplib

# Clone CF (CFDP — required by build config)
RUN git clone --depth 1 --branch ${CF_REF} \
        https://github.com/nasa/cf.git src/cf

# Build and install TinyCBOR
RUN git clone --depth 1 --branch ${TINYCBOR_REF} \
        https://github.com/intel/tinycbor.git && \
    cd tinycbor && make && make install PREFIX=/usr/local

# Build and install QCBOR (required by BPLib for CBOR encoding)
RUN git clone --depth 1 https://github.com/laurencelundblade/QCBOR.git && \
    cd QCBOR && make && make install PREFIX=/usr/local

# Inject STCP PSP module into the cFS PSP modules directory
COPY stcpsock_intf cfs-bundle/psp/fsw/modules/stcpsock_intf

# Apply AduUnwrapping fix (completing the existing TODO in fwp_adup.c)
RUN sed -i \
    '/TODO remove header/{s|/\* TODO remove header \*/|*AduSize -= sizeof(CFE_MSG_TelemetryHeader_t); AduPtr = (uint8 *)AduPtr + sizeof(CFE_MSG_TelemetryHeader_t);|}' \
    src/bpnode/fsw/fwp/fwp_adup.c

# Patch Contact 0 DestEIDs to route to Hardy (node 1) instead of default node 200
# Also widen the service range so any service on node 1 matches
RUN sed -i '0,/BPNODE_EID_NODE_NUM_FOR_CONTACT_0/{s/BPNODE_EID_NODE_NUM_FOR_CONTACT_0/1/}' \
    src/bpnode/fsw/tables/bpnode_contacts.c && \
    sed -i '0,/BPNODE_EID_NODE_NUM_FOR_CONTACT_0/{s/BPNODE_EID_NODE_NUM_FOR_CONTACT_0/1/}' \
    src/bpnode/fsw/tables/bpnode_contacts.c && \
    sed -i '0,/\.MaxService.*= BPNODE_EID_SERVICE_NUM_FOR_CONTACT_0/{s/\.MaxService.*= BPNODE_EID_SERVICE_NUM_FOR_CONTACT_0/.MaxService   = 65535/}' \
    src/bpnode/fsw/tables/bpnode_contacts.c && \
    sed -i '0,/\.MinService.*= BPNODE_EID_SERVICE_NUM_FOR_CONTACT_0/{s/\.MinService.*= BPNODE_EID_SERVICE_NUM_FOR_CONTACT_0/.MinService   = 0/}' \
    src/bpnode/fsw/tables/bpnode_contacts.c

# Enable AduWrapping on Channel 0 and AduUnwrapping on Channel 1 for SB echo routing
# Channel 1 DestEID → Hardy (node 1, service 128)
RUN sed -i '0,/\.AduWrapping.*= false/{s/\.AduWrapping.*= false/.AduWrapping            = true/}' \
    src/bpnode/fsw/tables/bpnode_channel.c && \
    sed -i 's/\.AduUnwrapping.*= false/.AduUnwrapping          = true/' \
    src/bpnode/fsw/tables/bpnode_channel.c && \
    sed -i '/Channel 1/,$ { s/\.Node.*= BPLIB_LOCAL_EID_NODE_NUM/.Node           = 1/; s/\.Service.*= BPNODE_EID_SERVICE_NUM_FOR_CHANNEL_1/.Service        = 128/ }' \
    src/bpnode/fsw/tables/bpnode_channel.c

# Shorten table names that exceed cFS default 16-char limit.
# Use word-boundary matching to avoid renaming type names like BPLib_NC_MIBConfigPNTable_t.
RUN sed -i \
    's/\bMIBConfigPNTable\b/MIBCfgPNTbl/g; s/\bMIBConfigPSTable\b/MIBCfgPSTbl/g' \
    src/bpnode/fsw/fwp/fwp_tablep.c \
    src/bpnode/fsw/tables/bpnode_mib_pn.c \
    src/bpnode/fsw/tables/bpnode_mib_ps.c

# Prepare build configuration
# Start from cFE sample_defs, then overlay BPNode's CI buildconfig
SHELL ["/bin/bash", "-c"]
RUN mkdir ./interop_defs && \
    cp -v cfs-bundle/cfe/cmake/sample_defs/cfe_perfids.h ./interop_defs/ && \
    cp -v -t ./interop_defs \
        cfs-bundle/cfe/cmake/sample_defs/*osconfig*.cmake \
        cfs-bundle/cfe/cmake/sample_defs/*custom*.cmake \
        cfs-bundle/cfe/cmake/sample_defs/*options*.cmake \
        cfs-bundle/cfe/cmake/sample_defs/*pspconfig*.cmake && \
    cp -v -t ./interop_defs \
        src/bpnode/.github/buildconfig/*.cmake \
        src/bpnode/.github/buildconfig/*.scr && \
    true
SHELL ["/bin/sh", "-c"]

# Custom sch_lab table with BPNode wakeup
COPY cfs-config/sch_lab_table.c      ./interop_defs/sch_lab_table.c

# Copy app topicids headers into mission defs (needed for table compilation)
RUN cp -v cfs-bundle/apps/ci_lab/fsw/inc/ci_lab_topicids.h ./interop_defs/ && \
    cp -v cfs-bundle/apps/to_lab/fsw/inc/to_lab_topicids.h ./interop_defs/

# Overlay our interop-specific configuration
COPY cfs-config/targets.cmake        ./interop_defs/targets.cmake
COPY cfs-config/bpnode_mission_cfg.h ./interop_defs/bpnode_mission_cfg.h
COPY cfs-config/cfe_es_startup.scr   ./interop_defs/cfe_es_startup.scr
COPY cfs-config/extra_install.cmake  ./interop_defs/extra_install.cmake
COPY cfs-config/install_custom.cmake ./interop_defs/install_custom.cmake

# Custom ADU proxy table for echo wiring
COPY cfs-config/bpnode_adup.c        src/bpnode/fsw/tables/bpnode_adup.c

# Run CMake
RUN cmake \
    -DCMAKE_BUILD_TYPE=Release \
    -DSIMULATION=native \
    -DCMAKE_INSTALL_PREFIX=/exe \
    -DMISSIONCONFIG=interop \
    -DMISSION_DEFS=$PWD/interop_defs \
    -B build \
    -S cfs-bundle/cfe

# Copy all app topicids headers into the generated include directory
RUN find cfs-bundle/apps src -name '*_topicids.h' -exec cp -v {} build/native/default/inc/ \;

# Build and install
RUN make -C build DESTDIR=/build -j${CORES} mission-install

##############################################
# Runtime
##############################################
FROM ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        libc6 \
        libsqlite3-0 \
        iproute2 \
        iputils-ping \
        net-tools \
        netcat-openbsd \
        procps \
    && rm -rf /var/lib/apt/lists/*

# Copy built cFS binaries and tables
COPY --from=builder /build/exe /opt/cfs

# Copy the host utilities (cmdUtil etc.)
COPY --from=builder /build/exe/host /opt/cfs/host

# Copy startup script to a known location
COPY cfs-config/cfe_es_startup.scr /opt/cfs/cfe_es_startup.scr

# Copy start script
COPY docker/start_cfs /usr/local/bin/start_cfs
RUN chmod +x /usr/local/bin/start_cfs

ENV PATH="${PATH}:/opt/cfs/host:/usr/local/bin"

WORKDIR /opt/cfs

ENTRYPOINT [ "start_cfs" ]
