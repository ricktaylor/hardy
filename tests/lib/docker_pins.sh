# Base-image digest pins for test builds of the Hardy production Dockerfiles.
#
# The production Dockerfiles (bpa-server/, tcpclv4-server/, tvr/, tools/) track
# floating tags, so released images pick up base-image updates on rebuild. Test
# harnesses (tests/image_checks.sh, tests/interop/run_all.sh) pin those bases by
# digest at build time — via pin_dockerfile below — so test results are
# reproducible and attributable to a known environment. The trixie-slim digest
# here is the same one the interop peer Dockerfiles pin inline, keeping every
# container in an interop run on one common base.
#
# To update a pin:
#   docker pull <image:tag>
#   docker image inspect --format '{{index .RepoDigests 0}}' <image:tag>

declare -A DOCKER_BASE_PINS=(
    ["gcr.io/distroless/cc-debian13"]="sha256:a017e74bd2a12d98342dbecd33d121d2b160415ed777573dc1808969e989d94d"
    ["debian:trixie-slim"]="sha256:28de0877c2189802884ccd20f15ee41c203573bd87bb6b883f5f46362d24c5c2"
)

# pin_dockerfile <dockerfile> — emit the Dockerfile on stdout with every
# `FROM <image>` naming a pinned base rewritten to `FROM <image>@<digest>`.
# Images already carrying a digest, and images not in the pin table, pass
# through unchanged.
pin_dockerfile() {
    local src="$1"
    local script="" image pattern
    for image in "${!DOCKER_BASE_PINS[@]}"; do
        pattern="${image//./\\.}"
        script+="s#^(FROM ${pattern})([[:space:]]|\$)#\\1@${DOCKER_BASE_PINS[$image]}\\2#;"
    done
    sed -E "$script" "$src"
}
