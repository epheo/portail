package conformance_test

import (
	"os"
	"strings"
	"testing"

	v1 "sigs.k8s.io/gateway-api/apis/v1"
	"sigs.k8s.io/gateway-api/apis/v1beta1"
	"sigs.k8s.io/gateway-api/conformance"
)

func TestConformance(t *testing.T) {
	opts := conformance.DefaultOptions(t)

	if addrs := os.Getenv("USABLE_ADDRESSES"); addrs != "" {
		opts.UsableNetworkAddresses = parseAddresses(addrs)
	}
	if addrs := os.Getenv("UNUSABLE_ADDRESSES"); addrs != "" {
		opts.UnusableNetworkAddresses = parseAddresses(addrs)
	}

	conformance.RunConformanceWithOptions(t, opts)
}

func parseAddresses(raw string) []v1beta1.GatewaySpecAddress {
	var addrs []v1beta1.GatewaySpecAddress
	ipType := v1.AddressType("IPAddress")
	for _, s := range strings.Split(raw, ",") {
		s = strings.TrimSpace(s)
		if s != "" {
			addrs = append(addrs, v1beta1.GatewaySpecAddress{
				Type:  &ipType,
				Value: s,
			})
		}
	}
	return addrs
}
