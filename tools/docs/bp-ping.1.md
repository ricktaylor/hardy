# BP-PING(1) - Bundle Protocol Ping

## NAME

bp ping - send ping bundles to a Bundle Protocol node

## SYNOPSIS

**bp ping** [*OPTIONS*] *DESTINATION* [*PEER*]

## DESCRIPTION

**bp ping** sends Bundle Protocol ping bundles to a destination endpoint and measures round-trip times. It provides network diagnostics similar to IP ping, including RTT statistics and packet loss percentage.

The tool embeds a minimal Bundle Protocol Agent (BPA) and establishes a direct TCPCLv4 connection to the specified peer. No external BPA deployment is required.

By default, bundles are signed with BIB-HMAC-SHA256 using a random session key. This detects payload corruption in transit. The echo service reflects bundles unchanged, so the same key verifies returning bundles.

Press Ctrl+C to stop and display summary statistics.

## ARGUMENTS

*DESTINATION*
:   The destination endpoint identifier (EID) to ping. This is the EID of the echo service, typically in IPN format (e.g., `ipn:2.7` or `ipn:2.2048.7`).

*PEER*
:   The TCPCLv4 peer address of the next hop. Format is `HOST:PORT` (e.g., `192.168.1.1:4556` or `node2.example.com:4556`). Required unless DNS-based EID resolution is available.

## OPTIONS

### Core Options

These options follow IP ping conventions.

**-c**, **--count** *N*
:   Stop after sending *N* ping bundles. Without this option, pings continue until interrupted.

**-i**, **--interval** *DURATION*
:   Wait *DURATION* between sending each ping. Default is `1s`. Accepts human-readable durations (e.g., `500ms`, `2s`, `1m`).

**-s**, **--size** *BYTES*
:   Target total bundle size in bytes. Padding is added to reach the exact size. Useful for MTU testing. If the minimum bundle size exceeds the target, an error is reported.

**-w**, **--timeout** *DURATION*
:   Stop the ping session after *DURATION* regardless of count. This is a deadline for the entire session.

**-W**, **--wait** *DURATION*
:   After sending the last ping (when using **-c**), wait up to *DURATION* for responses before printing statistics.

**-q**, **--quiet**
:   Suppress per-ping output. Only display the summary statistics at the end.

**-v**, **--verbose**[=*LEVEL*]
:   Enable verbose output. Without a value, defaults to `info`. Levels: `trace`, `debug`, `info`, `warn`, `error`. Useful for diagnosing BPA and CLA behaviour.

### DTN-Specific Options

**-t**, **--ttl** *LIMIT*
:   Add a HopCount extension block with the specified hop limit. Analogous to IP TTL - the bundle is discarded if it traverses more than *LIMIT* nodes.

**--lifetime** *DURATION*
:   Set the bundle lifetime (time-based expiry). If not specified, lifetime is calculated from session parameters to cover the expected ping duration.

**--no-sign**
:   Disable BIB-HMAC-SHA256 signing. By default, bundles are signed to detect corruption. Use this option for compatibility with echo services that may not preserve BIB blocks.

**-S**, **--source** *EID*
:   Use *EID* as the source endpoint identifier. If not specified, a random IPN EID is generated.

### TLS Options

**--tls-insecure**
:   Accept self-signed TLS certificates from the peer. Use only for testing.

**--tls-ca** *DIR*
:   Directory containing CA certificates for TLS validation. Certificates should be in PEM format.

## OUTPUT

Each successful ping response displays:

    Reply from ipn:2.7: seq=0 rtt=1.234s

If BIB verification fails, a warning is displayed and the response is counted as corrupted:

    WARNING: Ping 3 integrity check FAILED - payload corrupted!

Status reports from intermediate nodes show elapsed time from send (if status reporting is enabled on the network):

    Ping 0 received by ipn:3.0 after 230ms
    Ping 0 forwarded by ipn:3.0 after 234ms
    Ping 0 received by ipn:4.0 after 450ms
    Ping 0 forwarded by ipn:4.0 after 456ms
    Ping 0 delivered by ipn:2.7 after 567ms

When the reply arrives, the discovered path is displayed showing both received and forwarded times at each hop. This reveals store-and-forward delays:

    Reply from ipn:2.7: seq=0 rtt=1.234s
      path: ipn:3.0 (fwd 234ms, rcv 230ms) -> ipn:4.0 (fwd 456ms, rcv 450ms) -> ipn:2.7 (dlv 567ms)

## STATISTICS

On completion or Ctrl+C, summary statistics are displayed:

    --- ipn:2.7 ping statistics ---
    5 bundles transmitted, 4 received, 20% loss
    rtt min/avg/max/stddev = 1.234s/2.567s/4.891s/1.203s

If corruption was detected:

    --- ipn:2.7 ping statistics ---
    5 bundles transmitted, 4 received, 1 corrupted, 20% loss

Corrupted bundles are excluded from RTT statistics.

If status reports were received for lost bundles, their last known location is shown:

    Lost bundles last seen:
      seq=2 forwarded by ipn:4.0 after 456ms
      seq=3 deleted by ipn:4.0 after 456ms

This helps diagnose where bundles are being dropped or delayed in the network.

## EXIT STATUS

**0**
:   At least one response was received.

**1**
:   No responses were received, or an error occurred.

## EXAMPLES

Ping a node with default settings:

    bp ping ipn:2.7 192.168.1.1:4556

Send 10 pings at 500ms intervals:

    bp ping -c 10 -i 500ms ipn:2.7 192.168.1.1:4556

Test with 1KB bundles for MTU probing:

    bp ping -s 1024 ipn:2.7 192.168.1.1:4556

Ping with hop limit of 5 (like IP TTL):

    bp ping -t 5 ipn:2.7 192.168.1.1:4556

Quiet mode with timeout:

    bp ping -q -w 30s ipn:2.7 192.168.1.1:4556

Debug BPA behaviour:

    bp ping -v=debug ipn:2.7 192.168.1.1:4556

Connect to peer with self-signed certificate:

    bp ping --tls-insecure ipn:2.7 secure-node.example.com:4556

## INTEROPERABILITY

**bp ping** works with any echo service that reflects bundles unchanged:

- Hardy echo-service
- HDTN echo
- dtn7-rs dtnecho2
- uD3TN aap_echo

ION bpecho returns a fixed response rather than reflecting the payload, which breaks payload verification. Use **--no-sign** when testing against ION.

## SEE ALSO

ping(8), traceroute(8)

RFC 9171 (Bundle Protocol Version 7), RFC 9172 (Bundle Protocol Security)
