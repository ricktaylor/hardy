##############################################
# ESA-BP Interop Testing Docker Image
#
# Layers the STCP CLE on top of the native ESA-BP image
# for interoperability testing with Hardy.
#
# Requires the base ESA-BP image to be built first:
#   cd <esa-bp-source> && docker build -t esa-bp -f docker/Dockerfile .
#
# Then build this image (context = this directory):
#   docker build -t esa-bp-interop .
#
##############################################

ARG BASE_IMAGE=esa-bp

# Compile the STCP CLE against jars already in the base image
FROM ${BASE_IMAGE} AS compiler

# Need javac — base image only has JRE
USER root
RUN apt-get update && \
    apt-get install -y --no-install-recommends default-jdk && \
    rm -rf /var/lib/apt/lists/*

COPY stcp-cle/ClElementStcp.java /tmp/ClElementStcp.java
COPY stcp-cle/EchoService.java /tmp/EchoService.java

# node.jar is a shaded uber jar containing all ESA-BP classes;
# cli.jar contains the gRPC client stubs needed by EchoService
RUN mkdir -p /tmp/stcp-cle /tmp/echo-service && \
    NODE_JAR="$(find /opt/esa-bp -name 'node.jar' | head -1)" && \
    CLI_JAR="$(find /opt/esa-bp -name 'cli.jar' | head -1)" && \
    javac -d /tmp/stcp-cle -cp "$NODE_JAR" /tmp/ClElementStcp.java && \
    javac -d /tmp/echo-service -cp "$CLI_JAR:$NODE_JAR" /tmp/EchoService.java

# Final image: base + STCP CLE + our start script
FROM ${BASE_IMAGE}

USER root

COPY --from=compiler /tmp/stcp-cle/ /opt/esa-bp/interop-cle/
COPY --from=compiler /tmp/echo-service/ /opt/esa-bp/echo-service/
COPY docker/start_esa_bp /usr/local/bin/
RUN chmod +x /usr/local/bin/start_esa_bp

ENTRYPOINT ["start_esa_bp"]
