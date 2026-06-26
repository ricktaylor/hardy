##############################################
# ESA-BP Interop Testing Docker Image
#
# Layers the STCP CLE + echo service on top of the native ESA-BP image
# for interoperability testing with Hardy.
#
# Requires the base ESA-BP image to be built first (test_esa_bp_ping.sh does
# this: checkout -> strip-proprietary.sh -> Maven -> dist/bp-packager.zip ->
# docker build). Then build this image (context = this directory):
#   docker build -t esa-bp-interop --build-arg BASE_IMAGE=esa-bp .
##############################################

ARG BASE_IMAGE=esa-bp

# Alias the prebuilt base image as a stage, so COPY --from / the final FROM can
# reference it by name (the classic builder won't expand an ARG in COPY --from).
FROM ${BASE_IMAGE} AS base

# ---- Compile our CLE + echo service against the base image's jars. ----
# The base image is a JRE on Java 21, and node.jar holds Java-21 class files, so
# javac must also be Java 21 (an older JDK can't read them). Use a temurin 21 JDK
# and pull the jars in from the base image.
FROM eclipse-temurin:21-jdk-jammy AS compiler
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

# ---- Final image: base + compiled CLE/echo + our launcher. ----
FROM base
USER root

COPY --from=compiler /tmp/interop-cle/   /opt/esa-bp/interop-cle/
COPY --from=compiler /tmp/echo-service/  /opt/esa-bp/echo-service/
COPY docker/start_esa_bp /usr/local/bin/
RUN chmod +x /usr/local/bin/start_esa_bp

ENTRYPOINT ["start_esa_bp"]
