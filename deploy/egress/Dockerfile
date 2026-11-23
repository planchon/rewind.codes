# The nodes' egress gateway (egress.sh), the holders of their network namespaces (route.sh),
# the DNS relay of the nodes under gVisor (dns.sh) and the sandbox that checks their walls:
# Alpine with dnsmasq, ipset, iptables and iproute2.
FROM alpine:3.22
RUN apk add --no-cache dnsmasq ipset iptables iproute2
COPY egress.sh route.sh dns.sh /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/egress.sh"]
