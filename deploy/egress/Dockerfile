# The workers' egress gateway (egress.sh) and the holders of their network namespaces
# (route.sh): Alpine with dnsmasq, ipset, iptables and iproute2.
FROM alpine:3.22
RUN apk add --no-cache dnsmasq ipset iptables iproute2
COPY egress.sh route.sh /usr/local/bin/
ENTRYPOINT ["/usr/local/bin/egress.sh"]
