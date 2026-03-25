# Interop test targets.cmake — single CPU for echo testing with Hardy
#
# Based on .github/buildconfig/targets.cmake but simplified to one CPU

SET(MISSION_NAME "DTN")
SET(SPACECRAFT_ID 0xb9)

SET(MISSION_CPUNAMES echo)

list(APPEND MISSION_GLOBAL_APPLIST cf bpnode bplib ci_lab to_lab sch_lab)
set(GLOBAL_PSP_MODULELIST stcpsock_intf)

SET(echo_PROCESSORID 1)
SET(echo_PSP_MODULELIST ${GLOBAL_PSP_MODULELIST})
SET(echo_PLATFORM default)
