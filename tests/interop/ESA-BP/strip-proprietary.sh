#!/usr/bin/env bash
#
# Strip ESA-BP's proprietary space-link convergence layers (SLE + the
# generic-packetiser GP/EPP code) from a checked-out ESA-BP 3.0.0 source tree.
#
# Those CLs depend on gated ESA jars (esa.sle.java, esa.egos.generic.packetiser)
# that aren't publicly resolvable, and Hardy interop only needs the STCP CL — so
# we remove them, letting the node build entirely from open Maven repositories
# (no GitHub PAT, no ESA-GitLab access). Idempotent; safe to re-run.
#
# Usage: strip-proprietary.sh <esa-bp-source-dir>   (the dir containing ./src)
set -euo pipefail

ROOT="${1:?usage: strip-proprietary.sh <esa-bp-source-dir>}"
SRC="$ROOT/src"
core="$SRC/bp-convergence-layer-element-core/src/main/java/esa/egos/dtn/bp/convergence/layers/elements"
[ -d "$core" ] || { echo "ERROR: $core not found — is this an ESA-BP 3.0.0 tree?" >&2; exit 1; }

# 1. Delete the SLE + generic-packetiser/GP/EPP sources. This set is internally
#    self-contained: nothing outside it references these types except the two
#    files patched in step 2.
rm -f \
  "$core/ClElementSle.java" \
  "$core/utils/SleUtils.java" \
  "$core/ClElementGp.java" \
  "$core/config/genericPacketiser/BpGpMonitor.java" \
  "$core/config/genericPacketiser/FrameConfigAdapter.java" \
  "$core/config/genericPacketiser/FramePacketiserConfiguration.java" \
  "$core/config/genericPacketiser/PacketConfigAdapter.java" \
  "$core/config/genericPacketiser/PacketPacketiserConfiguration.java" \
  "$core/file/EppStreamParser.java"

# 2. StreamParserFactory: drop the EPP case (the only compile-time reference to
#    the deleted parser; EPP then falls through to the existing "not implemented"
#    default, which is fine — we never configure an EPP stream).
#    (UpperCleParser refers to ClElementGp only via a string case-label for
#    reflective dispatch, so it still compiles untouched.)
perl -0pi -e 's/\n[ \t]*case EPP:\n[ \t]*return new EppStreamParser\(\);//' \
  "$core/file/StreamParserFactory.java"

# 3. Drop the bp-sle module from the reactor.
perl -ni -e 'print unless m{^\s*<module>bp-sle</module>\s*$}' "$SRC/pom.xml"

# 4. Drop the bp-sle + generic-packetiser dependencies from the CL-element-core pom.
clepom="$SRC/bp-convergence-layer-element-core/pom.xml"
perl -0pi -e 's{\s*<dependency>\s*<groupId>esa\.egos\.dtn\.bp</groupId>\s*<artifactId>bp-sle</artifactId>\s*</dependency>}{}g' "$clepom"
perl -0pi -e 's{\s*<dependency>\s*<groupId>esa\.egos\.generic\.packetiser</groupId>\s*<artifactId>esa\.egos\.generic\.packetiser</artifactId>\s*</dependency>}{}g' "$clepom"

echo "Stripped ESA-BP proprietary CLs (SLE + generic-packetiser); node now builds from open Maven only."
