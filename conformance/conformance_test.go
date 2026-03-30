package conformance_test

import (
	"context"
	"fmt"
	"log"
	"net"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"

	v1 "sigs.k8s.io/gateway-api/apis/v1"
	"sigs.k8s.io/gateway-api/apis/v1beta1"
	"sigs.k8s.io/gateway-api/conformance"
	"sigs.k8s.io/kind/pkg/cluster"
)

const (
	clusterName       = "portail"
	gatewayAPIVersion = "v1.4.1"
	portailVersion    = "v0.1.0"
	portailImage      = "ghcr.io/epheo/portail:latest"
)

var kubeconfigPath string

// Usage:
//
//	cd conformance && go test -v -timeout 20m -count=1
func TestMain(m *testing.M) {
	if os.Getenv("SKIP_SETUP") != "" {
		os.Exit(m.Run())
	}

	runtime := detectRuntime()
	provider := cluster.NewProvider(kindProviderOption(runtime))
	cleanup := func() {
		if os.Getenv("SKIP_CLEANUP") == "" {
			log.Println("Deleting Kind cluster...")
			provider.Delete(clusterName, "")
		}
	}

	sig := make(chan os.Signal, 1)
	signal.Notify(sig, syscall.SIGINT, syscall.SIGTERM, syscall.SIGQUIT)
	go func() {
		<-sig
		cleanup()
		os.Exit(1)
	}()

	if err := setup(provider, runtime, cleanup); err != nil {
		log.Printf("Setup failed: %v", err)
		cleanup()
		os.Exit(1)
	}
}

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

func setup(provider *cluster.Provider, runtime string, cleanup func()) error {
	projectDir := filepath.Dir(mustGetwd())

	log.Println("Creating Kind cluster...")
	if err := provider.Create(clusterName, cluster.CreateWithWaitForReady(2*time.Minute)); err != nil {
		if !strings.Contains(err.Error(), "already exist") {
			return fmt.Errorf("create cluster: %w", err)
		}
	}
	kubeconfigPath = filepath.Join(projectDir, ".kubeconfig-kind")
	if err := provider.ExportKubeConfig(clusterName, kubeconfigPath, false); err != nil {
		return fmt.Errorf("export kubeconfig: %w", err)
	}
	os.Setenv("KUBECONFIG", kubeconfigPath)

	log.Println("Building and loading image...")
	if err := sh(runtime, "build", "--no-cache", "-t", portailImage, "-f", filepath.Join(projectDir, "Containerfile"), projectDir); err != nil {
		return err
	}
	archiveFile, err := os.CreateTemp("", "portail-image-*.tar")
	if err != nil {
		return fmt.Errorf("create temp archive: %w", err)
	}
	archive := archiveFile.Name()
	archiveFile.Close()
	defer os.Remove(archive)
	if err := sh(runtime, "save", "--output", archive, portailImage); err != nil {
		return err
	}
	nodes, err := provider.ListNodes(clusterName)
	if err != nil {
		return fmt.Errorf("list kind nodes: %w", err)
	}
	for _, node := range nodes {
		f, err := os.Open(archive)
		if err != nil {
			return err
		}
		// Import without --digests so the :latest tag is assigned from the archive.
		// kind's nodeutils.LoadImageArchive uses --digests which only stores digest
		// refs, causing kubelet to pull from the remote registry.
		cmd := node.Command("ctr", "--namespace=k8s.io", "images", "import", "--all-platforms", "-")
		cmd.SetStdin(f)
		err = cmd.Run()
		f.Close()
		if err != nil {
			return fmt.Errorf("load image into node %s: %w", node, err)
		}
	}

	log.Println("Installing Gateway API CRDs...")
	crdURL := fmt.Sprintf("https://github.com/kubernetes-sigs/gateway-api/releases/download/%s/experimental-install.yaml", gatewayAPIVersion)
	if err := kc("apply", "--server-side", "--force-conflicts", "-f", crdURL); err != nil {
		return err
	}

	log.Println("Installing MetalLB...")
	metallbURL := "https://raw.githubusercontent.com/metallb/metallb/v0.14.9/config/manifests/metallb-native.yaml"
	if err := kc("apply", "-f", metallbURL); err != nil {
		return err
	}
	if err := kc("wait", "-n", "metallb-system", "--for=condition=ready", "pod",
		"-l", "app=metallb,component=controller", "--timeout=120s"); err != nil {
		return fmt.Errorf("wait for metallb: %w", err)
	}

	poolRange, err := kindNetworkRange(runtime)
	if err != nil {
		return fmt.Errorf("detect kind network: %w", err)
	}
	log.Printf("MetalLB pool: %s", poolRange)
	poolFile, err := generatePoolYAML(poolRange)
	if err != nil {
		return fmt.Errorf("generate metallb pool: %w", err)
	}
	defer os.Remove(poolFile)
	if err := kc("apply", "-f", poolFile); err != nil {
		return err
	}

	log.Println("Deploying Portail...")
	manifestsDir := filepath.Join(mustGetwd(), "manifests")
	if err := kc("apply", "-k", manifestsDir); err != nil {
		return err
	}
	if err := kc("wait", "deployment/portail", "-n", "portail-system",
		"--for=condition=available", "--timeout=120s"); err != nil {
		return err
	}

	log.Println("Waiting for LoadBalancer VIP...")
	vip, err := waitForVIP("portail-system", "portail", 2*time.Minute)
	if err != nil {
		return err
	}
	log.Printf("VIP: %s", vip)

	out, err := exec.Command(runtime, "run", "--rm", portailImage, "--supported-features").Output()
	if err != nil {
		return fmt.Errorf("get supported features: %w", err)
	}
	features := strings.TrimSpace(string(out))
	log.Printf("Supported features: %s", features)

	// Execute tests on the Kind network: MetalLB L2 VIPs are only reachable
	// from inside the container network.
	code, err := execOnKindNetwork(provider, runtime, projectDir, vip, features)
	if err != nil {
		return err
	}
	cleanup()
	os.Exit(code)
	return nil
}

// execOnKindNetwork compiles the test binary and runs it inside a container
// on the Kind network so that MetalLB VIPs are directly reachable.
func execOnKindNetwork(provider *cluster.Provider, runtime, projectDir, vip, features string) (int, error) {
	conformanceDir := filepath.Join(projectDir, "conformance")
	reportDir := filepath.Join(conformanceDir, "reports")
	os.MkdirAll(reportDir, 0755)

	log.Println("Compiling conformance test binary...")
	testBin, err := os.CreateTemp("", "conformance-*.test")
	if err != nil {
		return 1, fmt.Errorf("create temp binary: %w", err)
	}
	testBin.Close()
	testBinPath := testBin.Name()

	build := exec.Command("go", "test", "-c", "-o", testBinPath, "./")
	build.Dir = conformanceDir
	build.Env = append(os.Environ(), "CGO_ENABLED=0")
	build.Stdout = os.Stdout
	build.Stderr = os.Stderr
	if err := build.Run(); err != nil {
		os.Remove(testBinPath)
		return 1, fmt.Errorf("compile test binary: %w", err)
	}
	defer os.Remove(testBinPath)

	internalKubeconfig, err := os.CreateTemp("", "kubeconfig-internal-*")
	if err != nil {
		return 1, fmt.Errorf("create internal kubeconfig: %w", err)
	}
	internalKubeconfig.Close()
	defer os.Remove(internalKubeconfig.Name())
	if err := provider.ExportKubeConfig(clusterName, internalKubeconfig.Name(), true); err != nil {
		return 1, fmt.Errorf("export internal kubeconfig: %w", err)
	}

	reportOutput := fmt.Sprintf("reports/%s/epheo-portail/experimental-%s-default-report.yaml", gatewayAPIVersion, portailVersion)

	log.Printf("Executing tests on Kind network (VIP %s)...", vip)
	cmd := exec.Command(runtime, "run", "--rm",
		"--network", "kind",
		"-v", testBinPath+":/conformance.test:Z",
		"-v", internalKubeconfig.Name()+":/kubeconfig:Z",
		"-v", reportDir+":/reports:Z",
		"-e", "SKIP_SETUP=1",
		"-e", "KUBECONFIG=/kubeconfig",
		"-e", "USABLE_ADDRESSES="+vip,
		"-e", "UNUSABLE_ADDRESSES=198.51.100.1",
		"registry.fedoraproject.org/fedora-minimal:43",
		"/conformance.test",
		"-test.v",
		"-test.timeout", "15m",
		"-test.count", "1",
		"-test.run", "TestConformance",
		"-gateway-class", "portail",
		"-conformance-profiles", "GATEWAY-HTTP,GATEWAY-TLS",
		"-supported-features", features,
		"-organization", "epheo",
		"-project", "portail",
		"-url", "https://github.com/epheo/portail",
		"-version", portailVersion,
		"-contact", "@epheo",
		"-report-output", "/"+reportOutput,
	)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	err = cmd.Run()
	if err != nil {
		if exitErr, ok := err.(*exec.ExitError); ok {
			log.Printf("Conformance tests exited with code %d (report: %s)", exitErr.ExitCode(), reportOutput)
			return exitErr.ExitCode(), nil
		}
		return 1, fmt.Errorf("run test container: %w", err)
	}
	log.Printf("Conformance tests passed (report: %s)", reportOutput)
	return 0, nil
}

func parseAddresses(raw string) []v1beta1.GatewaySpecAddress {
	var addrs []v1beta1.GatewaySpecAddress
	ipType := v1.AddressType("IPAddress")
	for _, s := range strings.Split(raw, ",") {
		if s = strings.TrimSpace(s); s != "" {
			addrs = append(addrs, v1beta1.GatewaySpecAddress{Type: &ipType, Value: s})
		}
	}
	return addrs
}

func detectRuntime() string {
	if rt := os.Getenv("CONTAINER_RUNTIME"); rt != "" {
		return rt
	}
	if _, err := exec.LookPath("podman"); err == nil {
		return "podman"
	}
	if _, err := exec.LookPath("docker"); err == nil {
		return "docker"
	}
	log.Fatal("No container runtime found (docker or podman)")
	return ""
}

func kindProviderOption(runtime string) cluster.ProviderOption {
	if runtime == "podman" {
		return cluster.ProviderWithPodman()
	}
	return cluster.ProviderWithDocker()
}

func kindNetworkRange(runtime string) (string, error) {
	var subnet string
	switch runtime {
	case "podman":
		out, err := exec.Command(runtime, "network", "inspect", "kind",
			"--format", `{{range .Subnets}}{{.Subnet}} {{end}}`).Output()
		if err != nil {
			return "", fmt.Errorf("inspect kind network: %w", err)
		}
		for _, s := range strings.Fields(string(out)) {
			if !strings.Contains(s, ":") {
				subnet = s
				break
			}
		}
	default:
		out, err := exec.Command(runtime, "network", "inspect", "kind",
			"-f", `{{(index .IPAM.Config 0).Subnet}}`).Output()
		if err != nil {
			return "", fmt.Errorf("inspect kind network: %w", err)
		}
		subnet = strings.TrimSpace(string(out))
	}

	if subnet == "" {
		return "", fmt.Errorf("no IPv4 subnet found in kind network")
	}

	_, ipnet, err := net.ParseCIDR(subnet)
	if err != nil {
		return "", fmt.Errorf("parse CIDR %q: %w", subnet, err)
	}

	broadcast := make(net.IP, len(ipnet.IP))
	for i := range broadcast {
		broadcast[i] = ipnet.IP[i] | ^ipnet.Mask[i]
	}
	start := make(net.IP, len(broadcast))
	end := make(net.IP, len(broadcast))
	copy(start, broadcast)
	copy(end, broadcast)
	start[len(start)-1] = broadcast[len(broadcast)-1] - 50
	end[len(end)-1] = broadcast[len(broadcast)-1] - 1
	return fmt.Sprintf("%s-%s", start, end), nil
}

func generatePoolYAML(addressRange string) (string, error) {
	const tpl = `apiVersion: metallb.io/v1beta1
kind: IPAddressPool
metadata:
  name: portail-pool
  namespace: metallb-system
spec:
  addresses:
  - %s
---
apiVersion: metallb.io/v1beta1
kind: L2Advertisement
metadata:
  name: portail-l2
  namespace: metallb-system
`
	path := filepath.Join(os.TempDir(), "metallb-pool.yaml")
	return path, os.WriteFile(path, []byte(fmt.Sprintf(tpl, addressRange)), 0644)
}

func waitForVIP(ns, name string, timeout time.Duration) (string, error) {
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	for {
		out, _ := kcOutput("get", "svc", name, "-n", ns,
			"-o", `jsonpath={.status.loadBalancer.ingress[0].ip}`)
		if vip := strings.TrimSpace(string(out)); vip != "" {
			return vip, nil
		}
		select {
		case <-ctx.Done():
			return "", fmt.Errorf("timed out waiting for VIP on %s/%s", ns, name)
		case <-time.After(2 * time.Second):
		}
	}
}

func kc(args ...string) error {
	return sh("kubectl", append([]string{"--kubeconfig", kubeconfigPath}, args...)...)
}

func kcOutput(args ...string) ([]byte, error) {
	return exec.Command("kubectl", append([]string{"--kubeconfig", kubeconfigPath}, args...)...).Output()
}

func sh(name string, args ...string) error {
	cmd := exec.Command(name, args...)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

func mustGetwd() string {
	wd, err := os.Getwd()
	if err != nil {
		log.Fatal(err)
	}
	return wd
}
