# Doctor and fallback discovery (roadmap step 10)

**Status:** complete and measured against both `labstream` and `liblsl`
outlets (`conformance/oracle/doctor_net.py`) and between Linux and Windows
over Wi-Fi. `docs/roadmap.md`, step 10, gives the success criterion.

## The purpose

Multicast discovery fails on many Wi-Fi access points and company networks.
When multicast fails, a liblsl inlet times out with no stream and no cause.
This step adds two parts:

1. **Fallback discovery in `labstream-net`.** When normal multicast finds no
   stream, the resolver tries directed subnet broadcast, local ARP neighbors,
   a local `/24` unicast probe, and TCP `LSL:shortinfo` for `KnownPeers`.
2. **`labstream doctor` (library API and CLI).** It tests the configuration,
   the interfaces, multicast, broadcast, live discovery paths, and an optional
   remote peer, and names the cause of each failure.

## What liblsl does today

These facts come from `conformance/liblsl/` at `e651023c`.

1. **Separate send and receive sockets.** `resolve_attempt_udp` opens four
   UDP sockets: `recv_socket_`, `unicast_socket_`, `broadcast_socket_`, and
   `multicast_socket_` (`src/resolve_attempt_udp.cpp:22-48`). The query
   carries the port of `recv_socket_` (`src/resolve_attempt_udp.cpp:57`).
   Because `recv_socket_` never sends a packet, a stateful firewall on
   Windows often drops the reply as unsolicited inbound UDP.
2. **Multicast and broadcast only, unless `KnownPeers` is set.** The
   resolver sends to the configured multicast addresses (with
   `255.255.255.255` in `multicast.LinkAddresses`,
   `src/api_config.cpp:198`), and to `lab.KnownPeers` over UDP
   (`src/resolver_impl.cpp:36-47`). It never tries directed subnet
   broadcast, ARP neighbors, or TCP `LSL:shortinfo`.
3. **Outlets answer unicast on port 16571 and on TCP.** The multicast
   responder binds `0.0.0.0:16571` and answers any `LSL:shortinfo` packet
   that arrives there, including unicast (`src/udp_server.cpp:48-67`,
   `src/udp_server.cpp:122-150`). The TCP server answers `LSL:shortinfo` on
   the data port (`src/tcp_server.cpp:502-555`).
4. **Interface enumeration.** liblsl enumerates interfaces with
   `GetAdaptersAddresses` on Windows and `getifaddrs` on Linux and macOS
   (`src/netinterfaces.cpp:21-114`). Until this step, `labstream-net` read
   only `/proc/net/fib_trie` and `/proc/net/if_inet6`, which returned no
   interface on Windows and macOS.

## Cross-platform interfaces and stateful-firewall replies

1. **`netif` module.** `labstream-net` enumerates active interfaces on Unix
   (Linux and macOS) with `getifaddrs`, and on Windows with
   `GetIpAddrTable`. Each IPv4 entry carries the address, prefix length,
   directed broadcast address, interface index, and flags.
2. **Replies on the sending port.** The resolver sends unicast queries
   (`KnownPeers`, ARP neighbors, and `/24` fallback) from `leg.rx` itself,
   so the source port equals `return_port`. The reply of the outlet then
   matches the stateful 5-tuple of the outgoing packet, and passes Windows
   Firewall with no inbound rule.

## Fallback discovery

### Configuration

`labstream.FallbackDiscovery` sets the fallback mode:

| Value | Behavior |
|---|---|
| `auto` (default) | Runs fallback only after wave 1 finds no stream |
| `off` | Runs classic LSL discovery only (multicast, `255.255.255.255`, `KnownPeers` UDP) |
| `subnet` | Runs the fallback paths on every wave, including wave 1 |

### The waves

1. **Wave 1 (normal LSL).** Sends to the configured multicast groups,
   `255.255.255.255`, and `lab.KnownPeers` over UDP. When multicast works,
   the stream answers in milliseconds and no fallback packet goes out.
2. **Wave 2 and later (fallback, when wave 1 found no stream).**
   - **Directed subnet broadcast.** Sends `LSL:shortinfo` to the directed
     broadcast address of each active non-loopback IPv4 interface on
     `multicast_port` (`16571`) and on `base_port..base_port + port_range`.
   - **ARP / neighbor cache unicast.** Reads IPv4 neighbors from
     `/proc/net/arp` on Linux and `GetIpNetTable` on Windows, and sends
     unicast `LSL:shortinfo` from `leg.rx` to `16571` and
     `base_port..base_port + port_range`. This works on any subnet size.
   - **Local `/24` unicast probe.** For each active non-loopback IPv4
     interface with prefix length 24 to 31 (at most 253 peers), sends
     unicast `LSL:shortinfo` from `leg.rx` to port `16571` and `base_port`
     (`16572`). Later waves also probe the next ports in the range. A
     subnet larger than `/24` is never swept.
   - **TCP `LSL:shortinfo` for `KnownPeers`.** When `lab.KnownPeers` is not
     empty and UDP found no stream, connects over TCP with a short timeout
     to `base_port..base_port + port_range` on each known peer and sends
     `LSL:shortinfo\r\n<query>\r\n`. This finds streams even when a
     firewall drops all UDP.

All four fallback paths use standard LSL packets, so they find both
`labstream` outlets and `liblsl` outlets with no wire extension.

## `labstream doctor`

### Library API and C API

- `labstream_net::doctor::diagnose(options)` runs the diagnostic checks and
  returns a `Report`. `labstream::doctor` re-exports it so Rust
  applications such as Phasic can run the doctor in process.
- `labstream-capi` exports `labstream_doctor_json` so Python and C
  applications can read the JSON report.

### CLI

`crates/labstream-cli` builds the `labstream` binary:

```sh
labstream doctor [--peer IP[,IP]] [--timeout SECONDS] [--json]
```

### The checks

1. **Configuration.** Shows the active configuration source, `ResolveScope`,
   `SessionID`, `Features`, `FallbackDiscovery`, `KnownPeers`, and ports.
2. **Interfaces.** Lists every detected IPv4 and IPv6 interface with its
   prefix, broadcast address, and flags. Warns if no non-loopback multicast
   interface exists, or if multiple non-loopback adapters exist.
3. **Multicast port.** Tests a shared bind on `ports.MulticastPort`
   (`16571`) with `SO_REUSEADDR`.
4. **Multicast self-test.** Sends a probe packet to `224.0.0.183:16571`
   through a local responder and checks whether it arrives.
5. **Broadcast self-test.** Sends a probe packet to `255.255.255.255:16571`
   and directed subnet broadcasts.
6. **Live discovery.** Runs a discovery pass and separates streams found by
   multicast from streams found only by fallback or `KnownPeers`. If
   multicast finds nothing while fallback finds a stream, the doctor names
   multicast blocking as the cause.
7. **Remote peer (`--peer IP`).** Tests three paths to each named peer:
   - UDP unicast from the receive port (stateful-firewall friendly),
   - UDP unicast from a separate port (detects firewalls that drop replies
     to a separate `return_port`),
   - TCP `LSL:shortinfo` on `base_port..base_port + port_range` (detects
     networks that block UDP while TCP works).

## Platform support

| Feature | Linux | Windows | macOS |
|---|---|---|---|
| IPv4 interfaces and subnet masks | `getifaddrs` | `GetIpAddrTable` (`iphlpapi`) | `getifaddrs` |
| IPv6 interface indexes | `getifaddrs` + `if_nametoindex` | `GetIpAddrTable` (`dwIndex`) | `getifaddrs` + `if_nametoindex` |
| Local ARP neighbors | `/proc/net/arp` | `GetIpNetTable` (`iphlpapi`) | empty list (directed broadcast and `/24` sweep still run) |
| Same-port UDP replies (`src_port == return_port`) | yes | yes (passes Windows Defender Firewall with no inbound rule) | yes |
| TCP `LSL:shortinfo` for `KnownPeers` | yes | yes | yes |

## Measured results (`conformance/oracle/doctor_net.py`)

Measured between two Linux network namespaces (`10.77.0.1/24` and
`10.77.0.2/24`) with `tc` filters that drop multicast, broadcast, or all UDP.

| Scenario | `labstream` outlet | `liblsl` oracle outlet |
|---|---|---|
| Multicast + `255.255.255.255` blocked, `FallbackDiscovery = off` | 0 streams in 1.004 s | 0 streams in 1.003 s |
| Multicast + `255.255.255.255` blocked, `FallbackDiscovery = auto` | 1 stream in 0.469 s | 1 stream in 0.472 s |
| Multicast + all broadcast blocked, cold ARP, `FallbackDiscovery = auto` | 1 stream in 0.626 s | 1 stream in 0.625 s |
| `labstream doctor --json` (multicast + all broadcast blocked) | `warn`, cause names multicast, stream via `fallback` | `warn`, cause names multicast, stream via `fallback` |
| All UDP blocked + `KnownPeers`, `FallbackDiscovery = auto` | 1 stream in 0.801 s | 1 stream in 0.780 s |
| `labstream doctor --peer 10.77.0.1 --json` (all UDP blocked) | `warn`, cause names UDP blocked while TCP works | `warn`, cause names UDP blocked while TCP works |

All 28 cells of `conformance/oracle/interop.py` also pass with
`FallbackDiscovery = auto`.

### Two machines (Linux and Windows over Wi-Fi)

Measured between Linux (`wlp1s0`, `192.168.2.208/24`) and Windows (`if8`,
`192.168.2.210/24`) with no `--peer` flag and no custom configuration:

- `labstream.exe doctor --peer 192.168.2.208` reported `OK` on all six checks
  (`interfaces`, `multicast_port`, `multicast_self_test` on `224.0.0.183:16571`,
  `broadcast_self_test`, `discovery`, and `peer:192.168.2.208` over UDP and TCP)
  and listed `Wifi_udp_512x8x1` on `192.168.2.208:16572` via multicast.
- `wifitest.exe inlet --mode udp --seconds 15 --loads 512x8x1` resolved the
  Linux outlet with no `--peer` flag and received 7680 of 7680 samples on UDP
  (0 lost, 100 % UDP, p99-p50 17.3 ms, max-p50 26.2 ms).

## Conformance plan

1. **Unit tests.** `netif` subnet host generation (never sweeps larger than
   `/24`), `/proc/net/arp` parser, `FallbackDiscovery` configuration
   parsing, `doctor::Report` formatting and JSON output.
2. **Blocked-multicast namespace test (`conformance/oracle/doctor_net.py`).**
   Two network namespaces on `10.77.0.1/24` and `10.77.0.2/24` with all
   multicast (`224.0.0.0/4`, `ff00::/8`) and `255.255.255.255` dropped:
   - Against a `labstream` outlet and against a `liblsl` oracle outlet:
     - `FallbackDiscovery = off` finds 0 streams.
     - `FallbackDiscovery = auto` finds the stream in less than 2 s.
     - `labstream doctor` names multicast blocking as the cause and lists
       the stream found by fallback.
   - With all UDP dropped and `lab.KnownPeers` set:
     - TCP `LSL:shortinfo` fallback finds both `labstream` and `liblsl`
       outlets in less than 2 s.
3. **Two machines (Linux and Windows).** Run `labstream doctor` on both
   machines and confirm interface detection, peer diagnosis, and discovery
   with no Windows Firewall rule for unicast.
4. **Existing test suite.** All workspace tests and `oracle/interop.py`
   keep passing with `FallbackDiscovery = auto`.
