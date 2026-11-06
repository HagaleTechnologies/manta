# MAN-471: multicast relay port sharing

The IPv4 multicast WSJT-X relay listener now sets `SO_REUSEADDR` before
binding `0.0.0.0:port`. Previously it bound with `std::net::UdpSocket`
without reuse, so another reuse-enabled consumer holding the port caused
`EADDRINUSE`, contrary to the shared-listener setup in `docs/CONFIG.md`.
This fixes the finding deferred from MAN-320, PR #439.

`socket2` moves from the CLI's dev dependencies to its normal dependencies.
The listener creates an IPv4 UDP socket, enables address reuse, binds the
wildcard address, joins the configured group on the unspecified interface,
and converts to a standard socket. The common nonblocking setup remains.
Unicast still uses a plain exclusive bind and keeps the same startup error.
`SO_REUSEPORT` is not needed for the ticket's reuse-enabled peer and is not
set. No configuration or spot output changes.

The Unix regression
`bind_wsjtx_socket_shares_a_multicast_port_with_a_reuse_enabled_peer`
keeps a peer bound to a kernel-assigned wildcard port with only
`SO_REUSEADDR` enabled. It checks the relay's successful shared binding,
wildcard address, matching port and nonblocking flag, and confirms the peer
remains bound. It uses no fixed port, sleep or network traffic.
On Linux it failed before the fix with `Address already in use` while the
three existing socket tests passed. After the fix, all four socket tests
and all three `relay_acceptance` tests passed. The test checks binding only, not
multicast packet delivery. macOS and Windows behavior require their CI
results; the new regression is Unix-only and does not prove Windows sharing.
