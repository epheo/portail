package conformance_test

import (
	"bufio"
	"context"
	"fmt"
	"net/http"
	"os"
	"sort"
	"strconv"
	"strings"
	"testing"
	"time"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/client-go/dynamic"
	"k8s.io/client-go/kubernetes"
	"k8s.io/client-go/tools/clientcmd"
	"sigs.k8s.io/yaml"
)

// What the controller costs per change it watches. CONTROLLER_BENCH=1
// runs it in place of the suite, on the Kind network like the suite:
//
//	cd conformance && CONTROLLER_BENCH=1 go test -v -timeout 90m -run TestConformance -count=1
//
// A: reconcile passes one event of each watched kind costs, at 1, 10 and
// 50 Gateways: an object no Gateway reads, then one only gw-0 reads. B:
// edit-to-served, from the apiserver accepting a change to the data plane
// answering by it. Only bench/gw-0 is Managed: the others are Unmanaged, so
// the node carries one data plane, and a pass of theirs translates and
// writes status but provisions nothing.

const (
	benchNS       = "bench"
	otherNS       = "bench-other"
	grantedNS     = "bench-granted"
	echoImage     = "registry.k8s.io/gateway-api/conformance/echo-basic:v0.1.0"
	settleQuiet   = 3 * time.Second
	settleAtLeast = 8 * time.Second
	eventRepeats  = 3
	// After Gateways are added: their Programmed condition settles over
	// the first half minute (a hold, then lapses), each a pass.
	scaledQuiet    = 10 * time.Second
	scaledAtLeast  = 40 * time.Second
	eventWindow    = 5 * time.Second
	servedPoll     = 10 * time.Millisecond
	servedTimeout  = 30 * time.Second
	servedSamples  = 20
	controllerPort = 19099
)

var benchSizes = []int{1, 10, 50}

// CONTROLLER_BENCH_SIZES (such as "1") narrows the Gateway counts run, and
// CONTROLLER_BENCH_ONLY=served skips A.
func sizes() []int {
	raw := os.Getenv("CONTROLLER_BENCH_SIZES")
	if raw == "" {
		return benchSizes
	}
	var out []int
	for _, f := range strings.Split(raw, ",") {
		n, err := strconv.Atoi(strings.TrimSpace(f))
		if err == nil && n > 0 {
			out = append(out, n)
		}
	}
	return out
}

var (
	gvrClass      = schema.GroupVersionResource{Group: "gateway.networking.k8s.io", Version: "v1", Resource: "gatewayclasses"}
	gvrNamespace  = schema.GroupVersionResource{Version: "v1", Resource: "namespaces"}
	gvrService    = schema.GroupVersionResource{Version: "v1", Resource: "services"}
	gvrSecret     = schema.GroupVersionResource{Version: "v1", Resource: "secrets"}
	gvrDeployment = schema.GroupVersionResource{Group: "apps", Version: "v1", Resource: "deployments"}
	gvrGateway    = schema.GroupVersionResource{Group: "gateway.networking.k8s.io", Version: "v1", Resource: "gateways"}
	gvrRoute      = schema.GroupVersionResource{Group: "gateway.networking.k8s.io", Version: "v1", Resource: "httproutes"}
	gvrGrant      = schema.GroupVersionResource{Group: "gateway.networking.k8s.io", Version: "v1beta1", Resource: "referencegrants"}
	gvrParams     = schema.GroupVersionResource{Group: "portail.epheo.eu", Version: "v1alpha1", Resource: "gatewayparameters"}
	gvrPolicy     = schema.GroupVersionResource{Group: "portail.epheo.eu", Version: "v1alpha1", Resource: "ratelimitpolicies"}
)

type bench struct {
	t    *testing.T
	ctx  context.Context
	dyn  dynamic.Interface
	core kubernetes.Interface
	vip  string
	// Per change kind, the samples over 2s by the last status they saw
	// before being served (0: no answer within the client's timeout).
	slowGaps map[int]int
	// Grows with each apply that must change its object: a value that
	// came back would make the apply a no-op, with no event.
	flip int
}

func TestControllerBench(t *testing.T) {
	if os.Getenv("CONTROLLER_BENCH") == "" {
		t.Skip("CONTROLLER_BENCH unset")
	}
	cfg, err := clientcmd.BuildConfigFromFlags("", os.Getenv("KUBECONFIG"))
	if err != nil {
		t.Fatal(err)
	}
	b := &bench{t: t, ctx: context.Background()}
	if b.dyn, err = dynamic.NewForConfig(cfg); err != nil {
		t.Fatal(err)
	}
	if b.core, err = kubernetes.NewForConfig(cfg); err != nil {
		t.Fatal(err)
	}
	b.setUp()

	var report strings.Builder
	report.WriteString("\n| Gateways | Event | Passes (median) | Runs |\n|---|---|---|---|\n")
	events := append(append([]eventKind(nil), unrelatedKinds...), readKinds...)
	served := "\n| Gateways | Change | Samples | p50 | p95 | Worst | Over 2s, last answer before |\n|---|---|---|---|---|---|---|\n"
	for _, n := range sizes() {
		b.scaleTo(n)
		for _, kind := range events {
			if os.Getenv("CONTROLLER_BENCH_ONLY") == "served" {
				break
			}
			runs := make([]int, eventRepeats)
			for r := range runs {
				runs[r] = b.passesFor(kind)
			}
			sorted := append([]int(nil), runs...)
			sort.Ints(sorted)
			fmt.Fprintf(&report, "| %d | %s | %d | %v |\n", n, kind.name, sorted[len(sorted)/2], runs)
		}
		for _, change := range []struct {
			name string
			run  func(int) time.Duration
		}{{"route edit", b.routeEditServed}, {"Service created", b.serviceCreateServed}} {
			took := make([]time.Duration, 0, servedSamples)
			b.slowGaps = map[int]int{}
			for i := 0; i < servedSamples; i++ {
				took = append(took, change.run(i))
			}
			sort.Slice(took, func(i, j int) bool { return took[i] < took[j] })
			at := func(q float64) time.Duration { return took[int(q*float64(len(took)-1)+0.5)] }
			served += fmt.Sprintf("| %d | %s | %d | %s | %s | %s | %v |\n", n, change.name, len(took),
				at(0.5).Round(time.Millisecond), at(0.95).Round(time.Millisecond),
				took[len(took)-1].Round(time.Millisecond), b.slowGaps)
		}
	}
	t.Log("A: reconcile passes per event" + report.String())
	t.Log("B: edit to served, polled every " + servedPoll.String() + served)
}

// --- Objects ---------------------------------------------------------------

func (b *bench) apply(gvr schema.GroupVersionResource, doc string) {
	b.t.Helper()
	obj := &unstructured.Unstructured{}
	if err := yaml.Unmarshal([]byte(doc), &obj.Object); err != nil {
		b.t.Fatalf("%s: %v", doc, err)
	}
	ri := b.dyn.Resource(gvr)
	var res dynamic.ResourceInterface = ri
	if ns := obj.GetNamespace(); ns != "" {
		res = ri.Namespace(ns)
	}
	if _, err := res.Apply(b.ctx, obj.GetName(), obj, metav1.ApplyOptions{FieldManager: "bench", Force: true}); err != nil {
		b.t.Fatalf("apply %s %s: %v", gvr.Resource, obj.GetName(), err)
	}
}

func (b *bench) remove(gvr schema.GroupVersionResource, ns, name string) {
	b.t.Helper()
	if err := b.dyn.Resource(gvr).Namespace(ns).Delete(b.ctx, name, metav1.DeleteOptions{}); err != nil {
		b.t.Fatalf("delete %s %s: %v", gvr.Resource, name, err)
	}
}

func (b *bench) setUp() {
	for _, ns := range []string{benchNS, otherNS, grantedNS} {
		b.apply(gvrNamespace, fmt.Sprintf("apiVersion: v1\nkind: Namespace\nmetadata: {name: %s}\n", ns))
	}
	b.apply(gvrDeployment, fmt.Sprintf(`apiVersion: apps/v1
kind: Deployment
metadata: {name: echo, namespace: %s}
spec:
  replicas: 1
  selector: {matchLabels: {app: echo}}
  template:
    metadata: {labels: {app: echo}}
    spec:
      containers:
      - name: echo
        image: %s
        env:
        - name: POD_NAME
          valueFrom: {fieldRef: {fieldPath: metadata.name}}
        - name: NAMESPACE
          valueFrom: {fieldRef: {fieldPath: metadata.namespace}}
`, benchNS, echoImage))
	b.apply(gvrParams, fmt.Sprintf(`apiVersion: portail.epheo.eu/v1alpha1
kind: GatewayParameters
metadata: {name: unmanaged, namespace: %s}
spec: {provisioning: Unmanaged}
`, benchNS))
	// The objects only gw-0 reads, one per kind, and those no Gateway of
	// this controller reads.
	b.touchOwnParams()
	b.touchGrant()
	b.apply(gvrService, serviceDoc("granted", grantedNS, "nothing", 0))
	b.applyUnrelated()
}

// A Service on port 8080 selecting `app`, its bench-flip label `flip`.
func serviceDoc(name, ns, app string, flip int) string {
	return fmt.Sprintf(`apiVersion: v1
kind: Service
metadata: {name: %s, namespace: %s, labels: {bench-flip: "%d"}}
spec:
  selector: {app: %s}
  ports: [{port: 8080, targetPort: 3000}]
`, name, ns, flip, app)
}

// Gateway i, its route and the Service it routes to. gw-0 is Managed, its
// route serves /base, the others serve every path. gw-0 alone reads a
// namespace (its listener selects routes by label), its own parameters,
// and a grant (a rule routes to another namespace).
func (b *bench) addGateway(i int) {
	params, listener := "unmanaged", "{name: http, port: 80, protocol: HTTP}"
	if i == 0 {
		params = "gw-0"
		listener = fmt.Sprintf("{name: http, port: 80, protocol: HTTP, allowedRoutes: {namespaces: {from: Selector, selector: {matchLabels: {kubernetes.io/metadata.name: %s}}}}}", benchNS)
	}
	b.apply(gvrGateway, fmt.Sprintf(`apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: gw-%d, namespace: %s}
spec:
  gatewayClassName: portail
  infrastructure: {parametersRef: {group: portail.epheo.eu, kind: GatewayParameters, name: %s}}
  listeners:
  - %s
`, i, benchNS, params, listener))
	b.apply(gvrService, serviceDoc(fmt.Sprintf("svc-%d", i), benchNS, "echo", 0))
	path := "/"
	if i == 0 {
		path = "/base"
	}
	b.apply(gvrRoute, b.route(i, path, nil))
}

// gw-i's route: its base rule, then `extra` rules as (path, Service).
func (b *bench) route(i int, base string, extra [][2]string) string {
	rule := func(path, svc string) string {
		return fmt.Sprintf("  - matches: [{path: {type: PathPrefix, value: %q}}]\n    backendRefs: [{name: %s, port: 8080}]\n", path, svc)
	}
	rules := rule(base, fmt.Sprintf("svc-%d", i))
	if i == 0 {
		rules += fmt.Sprintf("  - matches: [{path: {type: PathPrefix, value: /granted}}]\n    backendRefs: [{name: granted, namespace: %s, port: 8080}]\n", grantedNS)
	}
	for _, e := range extra {
		rules += rule(e[0], e[1])
	}
	return fmt.Sprintf(`apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: rt-%d, namespace: %s}
spec:
  parentRefs: [{name: gw-%d}]
  rules:
%s`, i, benchNS, i, rules)
}

func (b *bench) scaleTo(n int) {
	b.t.Helper()
	have := b.gatewayCount()
	for i := have; i < n; i++ {
		b.addGateway(i)
	}
	for i := 0; i < n; i++ {
		b.waitGatewayAccepted(fmt.Sprintf("gw-%d", i))
	}
	if b.vip == "" {
		b.vip = b.waitVIP("gw-0")
		b.waitStatus("http://"+b.vip+"/base", 200)
	}
	b.settleFor(scaledQuiet, scaledAtLeast)
}

func (b *bench) gatewayCount() int {
	list, err := b.dyn.Resource(gvrGateway).Namespace(benchNS).List(b.ctx, metav1.ListOptions{})
	if err != nil {
		b.t.Fatal(err)
	}
	return len(list.Items)
}

func (b *bench) waitGatewayAccepted(name string) {
	b.t.Helper()
	deadline := time.Now().Add(2 * time.Minute)
	for time.Now().Before(deadline) {
		gw, err := b.dyn.Resource(gvrGateway).Namespace(benchNS).Get(b.ctx, name, metav1.GetOptions{})
		if err == nil && hasCondition(gw, "Accepted", "True") {
			return
		}
		time.Sleep(200 * time.Millisecond)
	}
	b.t.Fatalf("Gateway %s not Accepted in 2m", name)
}

func hasCondition(obj *unstructured.Unstructured, kind, status string) bool {
	conditions, _, _ := unstructured.NestedSlice(obj.Object, "status", "conditions")
	for _, c := range conditions {
		m, _ := c.(map[string]interface{})
		if m["type"] == kind && m["status"] == status {
			return true
		}
	}
	return false
}

func (b *bench) waitVIP(name string) string {
	b.t.Helper()
	deadline := time.Now().Add(3 * time.Minute)
	for time.Now().Before(deadline) {
		gw, err := b.dyn.Resource(gvrGateway).Namespace(benchNS).Get(b.ctx, name, metav1.GetOptions{})
		if err == nil && hasCondition(gw, "Programmed", "True") {
			addrs, _, _ := unstructured.NestedSlice(gw.Object, "status", "addresses")
			for _, a := range addrs {
				if m, ok := a.(map[string]interface{}); ok && m["value"] != "" {
					return fmt.Sprint(m["value"])
				}
			}
		}
		time.Sleep(500 * time.Millisecond)
	}
	b.t.Fatalf("Gateway %s not Programmed with an address in 3m", name)
	return ""
}

// --- A: passes per unrelated event ----------------------------------------

type eventKind struct {
	name  string
	touch func(*bench)
}

// One unrelated object per watched kind: what changing it costs.
var unrelatedKinds = []eventKind{
	{"none (control)", func(*bench) {}},
	{"Service", func(b *bench) { b.applyUnrelatedService() }},
	{"Secret", func(b *bench) { b.applyUnrelatedSecret() }},
	{"Namespace label", func(b *bench) { b.applyUnrelatedNamespace() }},
	{"ReferenceGrant", func(b *bench) { b.applyUnrelatedGrant() }},
	{"HTTPRoute, another Gateway's", func(b *bench) { b.applyUnrelatedRoute() }},
	{"GatewayParameters", func(b *bench) { b.applyUnrelatedParams() }},
	{"RateLimitPolicy", func(b *bench) { b.applyUnrelatedPolicy() }},
}

// One object per kind that gw-0 alone reads: each must wake it alone.
var readKinds = []eventKind{
	{"Service, gw-0's backend", func(b *bench) { b.touchBackend() }},
	{"Namespace label, gw-0's Selector read it", func(b *bench) { b.touchSelected() }},
	{"ReferenceGrant, where gw-0 routes", func(b *bench) { b.touchGrant() }},
	{"GatewayParameters, gw-0's own", func(b *bench) { b.touchOwnParams() }},
}

func (b *bench) touchBackend() {
	b.flip++
	b.apply(gvrService, serviceDoc("svc-0", benchNS, "echo", b.flip))
}

// The namespace of every Gateway: only gw-0's listener selects by label.
func (b *bench) touchSelected() {
	b.flip++
	b.apply(gvrNamespace, fmt.Sprintf("apiVersion: v1\nkind: Namespace\nmetadata: {name: %s, labels: {bench-flip: \"%d\"}}\n", benchNS, b.flip))
}

func (b *bench) touchGrant() {
	b.flip++
	b.apply(gvrGrant, fmt.Sprintf(`apiVersion: gateway.networking.k8s.io/v1beta1
kind: ReferenceGrant
metadata: {name: allow, namespace: %s, labels: {bench-flip: "%d"}}
spec:
  from: [{group: gateway.networking.k8s.io, kind: HTTPRoute, namespace: %s}]
  to: [{group: "", kind: Service}]
`, grantedNS, b.flip, benchNS))
}

func (b *bench) touchOwnParams() {
	b.flip++
	b.apply(gvrParams, fmt.Sprintf(`apiVersion: portail.epheo.eu/v1alpha1
kind: GatewayParameters
metadata: {name: gw-0, namespace: %s, labels: {bench-flip: "%d"}}
spec: {provisioning: Managed}
`, benchNS, b.flip))
}

func (b *bench) applyUnrelated() {
	for _, k := range unrelatedKinds {
		k.touch(b)
	}
}

func (b *bench) applyUnrelatedService() {
	b.flip++
	b.apply(gvrService, fmt.Sprintf(`apiVersion: v1
kind: Service
metadata: {name: unrelated, namespace: %s}
spec:
  selector: {app: nothing}
  ports: [{port: %d, targetPort: 3000}]
`, otherNS, 10000+b.flip))
}

func (b *bench) applyUnrelatedSecret() {
	b.flip++
	b.apply(gvrSecret, fmt.Sprintf(`apiVersion: v1
kind: Secret
metadata: {name: unrelated, namespace: %s}
stringData: {value: "%d"}
`, otherNS, b.flip))
}

func (b *bench) applyUnrelatedNamespace() {
	b.flip++
	b.apply(gvrNamespace, fmt.Sprintf("apiVersion: v1\nkind: Namespace\nmetadata: {name: %s, labels: {bench-flip: \"%d\"}}\n", otherNS, b.flip))
}

func (b *bench) applyUnrelatedGrant() {
	b.flip++
	b.apply(gvrGrant, fmt.Sprintf(`apiVersion: gateway.networking.k8s.io/v1beta1
kind: ReferenceGrant
metadata: {name: unrelated, namespace: %s}
spec:
  from: [{group: gateway.networking.k8s.io, kind: HTTPRoute, namespace: nowhere-%d}]
  to: [{group: "", kind: Service}]
`, otherNS, b.flip))
}

func (b *bench) applyUnrelatedRoute() {
	b.flip++
	b.apply(gvrRoute, fmt.Sprintf(`apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: unrelated, namespace: %s}
spec:
  parentRefs: [{name: nowhere}]
  rules: [{matches: [{path: {type: PathPrefix, value: "/flip-%d"}}]}]
`, otherNS, b.flip))
}

func (b *bench) applyUnrelatedParams() {
	b.flip++
	b.apply(gvrParams, fmt.Sprintf(`apiVersion: portail.epheo.eu/v1alpha1
kind: GatewayParameters
metadata: {name: unrelated, namespace: %s, labels: {bench-flip: "%d"}}
spec: {replicas: 1}
`, otherNS, b.flip))
}

func (b *bench) applyUnrelatedPolicy() {
	b.flip++
	b.apply(gvrPolicy, fmt.Sprintf(`apiVersion: portail.epheo.eu/v1alpha1
kind: RateLimitPolicy
metadata: {name: unrelated, namespace: %s}
spec:
  targetRef: {group: gateway.networking.k8s.io, kind: Gateway, name: nowhere}
  requestsPerSecond: %d
`, otherNS, 10+b.flip))
}

// The passes one event of `kind` costs. A Gateway that is not converged
// (the Unmanaged ones, never Programmed) re-checks on a 60s cadence: a
// change every Gateway re-runs on first lines those cadences up, then the
// controller is let settle, and the event lands in the quiet that follows.
func (b *bench) passesFor(kind eventKind) int {
	b.align()
	b.settle()
	before := b.passes()
	kind.touch(b)
	time.Sleep(eventWindow)
	return b.passes() - before
}

// A label on the GatewayClass: every Gateway of the class re-runs.
func (b *bench) align() {
	b.t.Helper()
	b.flip++
	patch := fmt.Sprintf(`{"metadata": {"labels": {"bench-align": "%d"}}}`, b.flip)
	if _, err := b.dyn.Resource(gvrClass).Patch(b.ctx, "portail", types.MergePatchType, []byte(patch), metav1.PatchOptions{}); err != nil {
		b.t.Fatalf("label the GatewayClass: %v", err)
	}
}

// Wait until the pass count has not moved for settleQuiet, and at least
// settleAtLeast: a pass that wrote status re-runs 5s later.
func (b *bench) settle() { b.settleFor(settleQuiet, settleAtLeast) }

func (b *bench) settleFor(quiet, atLeast time.Duration) {
	start, last, seen := time.Now(), time.Now(), b.passes()
	for {
		time.Sleep(500 * time.Millisecond)
		now := b.passes()
		if now != seen {
			seen, last = now, time.Now()
			continue
		}
		if time.Since(last) >= quiet && time.Since(start) >= atLeast {
			return
		}
	}
}

// Every pass of every controller replica so far: the count of the pass
// duration histogram, read through the apiserver's pod proxy.
func (b *bench) passes() int {
	b.t.Helper()
	pods, err := b.core.CoreV1().Pods(controllerNS).List(b.ctx, metav1.ListOptions{
		LabelSelector: "app.kubernetes.io/name=portail-controller",
	})
	if err != nil {
		b.t.Fatal(err)
	}
	total := 0
	for _, pod := range pods.Items {
		raw, err := b.core.CoreV1().Pods(controllerNS).
			ProxyGet("http", pod.Name, strconv.Itoa(controllerPort), "/metrics", nil).DoRaw(b.ctx)
		if err != nil {
			b.t.Fatalf("metrics of %s: %v", pod.Name, err)
		}
		scan := bufio.NewScanner(strings.NewReader(string(raw)))
		for scan.Scan() {
			line := scan.Text()
			if strings.HasPrefix(line, "portail_controller_reconcile_duration_seconds_count ") {
				n, _ := strconv.Atoi(strings.TrimSpace(strings.TrimPrefix(line, "portail_controller_reconcile_duration_seconds_count ")))
				total += n
			}
		}
	}
	return total
}

// --- B: edit to served -------------------------------------------------------

// Short against servedPoll's grain: a request to a backend not reachable
// yet (a ClusterIP kube-proxy has not programmed) hangs, and a long timeout
// would round its sample up by the timeout.
var httpClient = &http.Client{Timeout: 250 * time.Millisecond}

func status(url string) int {
	resp, err := httpClient.Get(url)
	if err != nil {
		return 0
	}
	resp.Body.Close()
	return resp.StatusCode
}

// The time from `start` until `url` first answers `want`.
func (b *bench) waitStatus(url string, want int) time.Duration {
	took, _ := b.waitStatusSeen(url, want)
	return took
}

// Like waitStatus, with the last other status seen meanwhile (0: none, or
// no answer).
func (b *bench) waitStatusSeen(url string, want int) (time.Duration, int) {
	b.t.Helper()
	start, last := time.Now(), 0
	for time.Since(start) < servedTimeout {
		got := status(url)
		if got == want {
			return time.Since(start), last
		}
		last = got
		time.Sleep(servedPoll)
	}
	b.t.Fatalf("%s did not answer %d in %s", url, want, servedTimeout)
	return 0, 0
}

func (b *bench) noteGap(took time.Duration, last int) {
	if took > 2*time.Second {
		b.slowGaps[last]++
	}
}

// A rule added to gw-0's route: from the apply to its path served.
func (b *bench) routeEditServed(k int) time.Duration {
	path := fmt.Sprintf("/edit-%d", k)
	url := "http://" + b.vip + path
	b.waitStatus(url, 404)
	start := time.Now()
	b.apply(gvrRoute, b.route(0, "/base", [][2]string{{path, "svc-0"}}))
	_, last := b.waitStatusSeen(url, 200)
	took := time.Since(start)
	b.noteGap(took, last)
	b.apply(gvrRoute, b.route(0, "/base", nil))
	return took
}

// A rule whose Service does not exist yet: from the Service's create to
// its path served.
func (b *bench) serviceCreateServed(k int) time.Duration {
	path, svc := fmt.Sprintf("/svc-%d", k), fmt.Sprintf("late-%d", k)
	url := "http://" + b.vip + path
	b.apply(gvrRoute, b.route(0, "/base", [][2]string{{path, svc}}))
	b.waitStatus(url, 500)
	start := time.Now()
	b.apply(gvrService, fmt.Sprintf(`apiVersion: v1
kind: Service
metadata: {name: %s, namespace: %s}
spec:
  selector: {app: echo}
  ports: [{port: 8080, targetPort: 3000}]
`, svc, benchNS))
	_, last := b.waitStatusSeen(url, 200)
	took := time.Since(start)
	b.noteGap(took, last)
	b.apply(gvrRoute, b.route(0, "/base", nil))
	b.remove(gvrService, benchNS, svc)
	return took
}
