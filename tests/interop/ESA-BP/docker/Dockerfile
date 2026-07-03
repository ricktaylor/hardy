##############################################
# ESA-BP Interop Testing Docker Image
#
# Runs ESA-BP's built node distribution on the shared debian:trixie-slim base
# (Debian's OpenJDK 21), with our STCP CLE + echo service layered in — so ESA-BP
# executes on the same pinned runtime as the other peers.
#
# Requires the base ESA-BP image to be built first (test_esa_bp_ping.sh does
# this: checkout -> strip-proprietary.sh -> Maven -> dist/bp-packager.zip ->
# docker build). Then build this image (context = this directory):
#   docker build -t esa-bp-interop --build-arg BASE_IMAGE=esa-bp .
##############################################

ARG BASE_IMAGE=esa-bp

# The prebuilt esa-bp image is used only as a source of the built node
# distribution (/opt/esa-bp); the interop image compiles and runs on the shared,
# pinned debian:trixie-slim base like the other peers. (Aliased as a stage because
# the classic builder won't expand an ARG in COPY --from.)
FROM ${BASE_IMAGE} AS base

# ---- Compile our CLE + echo service against the node's jars. ----
# node.jar holds Java-21 class files, so javac must be Java 21; Debian trixie's
# openjdk-21-jdk-headless provides it. The jars come from the base image.
FROM debian:trixie-slim AS compiler
RUN apt-get update && apt-get install -y --no-install-recommends openjdk-21-jdk-headless \
    && rm -rf /var/lib/apt/lists/*
COPY --from=base /opt/esa-bp /opt/esa-bp

COPY stcp-cle/ClElementStcp.java /tmp/ClElementStcp.java
COPY stcp-cle/EchoService.java   /tmp/EchoService.java

# node.jar is a shaded uber jar with the node + gRPC stubs; cli.jar holds the
# gRPC client stubs for the echo service. Both ship under <module>/jars/.
RUN set -eux; \
    mkdir -p /tmp/interop-cle /tmp/echo-service; \
    NODE_JAR="$(find /opt/esa-bp -name 'node.jar' | head -1)"; \
    CLI_JAR="$(find /opt/esa-bp -path '*bp-cli*' -name '*.jar' ! -name '*-sources.jar' ! -name '*-javadoc.jar' | head -1)"; \
    echo "NODE_JAR=$NODE_JAR  CLI_JAR=$CLI_JAR"; \
    [ -n "$NODE_JAR" ] || { echo "node.jar not found in base image"; exit 1; }; \
    javac -d /tmp/interop-cle  -cp "$NODE_JAR" /tmp/ClElementStcp.java; \
    javac -d /tmp/echo-service -cp "${CLI_JAR:+$CLI_JAR:}$NODE_JAR" /tmp/EchoService.java

# ---- Final image: trixie + OpenJDK 21 JRE + node dist + CLE/echo + launcher. ----
FROM debian:trixie-slim@sha256:28de0877c2189802884ccd20f15ee41c203573bd87bb6b883f5f46362d24c5c2

RUN apt-get update && apt-get install -y --no-install-recommends openjdk-21-jre-headless \
    && rm -rf /var/lib/apt/lists/*

COPY --from=base     /opt/esa-bp        /opt/esa-bp
COPY --from=compiler /tmp/interop-cle/  /opt/esa-bp/interop-cle/
COPY --from=compiler /tmp/echo-service/ /opt/esa-bp/echo-service/
COPY docker/start_esa_bp /usr/local/bin/
RUN chmod +x /usr/local/bin/start_esa_bp

# Bake the tested version (captured from the pinned ref before strip-proprietary.sh
# ran) into /interop-version, so run_all.sh reads it uniformly with the other peers.
ARG INTEROP_VERSION=
RUN printf '%s\n' "${INTEROP_VERSION:-source build}" > /interop-version

WORKDIR /opt/esa-bp
ENTRYPOINT ["start_esa_bp"]
