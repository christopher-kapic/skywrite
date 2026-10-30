These are primary IANA IPv4 and IPv6 Special-Purpose Address Registry CSVs,
retrieved 2026-10-30 from:
https://www.iana.org/assignments/iana-ipv4-special-registry/iana-ipv4-special-registry-1.csv
https://www.iana.org/assignments/iana-ipv6-special-registry/iana-ipv6-special-registry-1.csv
Registry pages last updated 2025-10-09 at review time.

endpoints.txt records the first and last address of every row. Expected admission
is a conservative subset of Globally Reachable=True; whole 192.0.0.0/16,
192.88.99.0/24, 2001::/23, 2002::/16 and addresses outside IPv6 2000::/3
remain denied. False, N/A and terminated entries are denied. Multicast is denied.
No runtime fetch occurs. Registry evolution requires reviewing this fixture.

TLS files are public, deliberately fake TEST ONLY credentials generated solely
for owned loopback tests. They are never used by production transport.
