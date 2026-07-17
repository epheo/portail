window.BENCHMARK_DATA = {
  "lastUpdate": 1784331712305,
  "repoUrl": "https://github.com/epheo/portail",
  "entries": {
    "Benchmark": [
      {
        "commit": {
          "author": {
            "email": "root@epheo.eu",
            "name": "Thibaut Lapierre",
            "username": "epheo"
          },
          "committer": {
            "email": "github@epheo.eu",
            "name": "Thibaut Lapierre",
            "username": "epheo"
          },
          "distinct": true,
          "id": "04cf0e3199f9b30df19f443d4f1b4defacefeaa0",
          "message": "Unscoped mode: class ownership filter, per-Gateway state, serialized apply\n\nOne process watching every Gateway had three faults the scoped path\nnever hits:\n\n- No GatewayClass filtering: portail reconciled Gateways belonging\n  to other controllers - status writes fighting the real owner,\n  ports bound for traffic it was never asked to serve. Unscoped\n  mode now opens a GatewayClass watch and ignores any Gateway whose\n  class does not name this controller (an absent class is not ours\n  to claim; a class watch re-triggers when ownership changes).\n\n- One shared last_applied fingerprint slot: two Gateways'\n  alternating passes never matched it, so the short-circuit never\n  fired and every requeue re-ran the rebuild and all status\n  PATCHes. The fingerprint is now keyed per Gateway and pruned with\n  the config map.\n\n- Concurrent reconciles could interleave merge/build/swap: a pass\n  building from a stale merge could store its table over a newer\n  one, and with listener teardown could close a just-added sibling\n  listener. The apply section now runs under a process-wide async\n  lock; a later pass re-merges from the map already holding the\n  earlier insert. Uncontended in scoped mode.",
          "timestamp": "2026-07-18T01:20:05+02:00",
          "tree_id": "5028eedf6dcf905c45490c66859d5d2a44245096",
          "url": "https://github.com/epheo/portail/commit/04cf0e3199f9b30df19f443d4f1b4defacefeaa0"
        },
        "date": 1784331711523,
        "tool": "cargo",
        "benches": [
          {
            "name": "e2e_http_request_processing",
            "value": 736,
            "range": "± 16",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_auth_request",
            "value": 141,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_product_search",
            "value": 230,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_health_check_processing",
            "value": 93,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_404_route_processing",
            "value": 192,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "throughput_simulation_1000_requests",
            "value": 382272,
            "range": "± 7586",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_simple_get",
            "value": 102,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_auth",
            "value": 140,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_cookies",
            "value": 133,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_complex_path",
            "value": 243,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple",
            "value": 58,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical",
            "value": 171,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex",
            "value": 512,
            "range": "± 16",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple_repeated",
            "value": 58,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical_repeated",
            "value": 170,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex_repeated",
            "value": 512,
            "range": "± 52",
            "unit": "ns/iter"
          },
          {
            "name": "parse_malformed_request",
            "value": 62,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/100",
            "value": 117,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/500",
            "value": 752,
            "range": "± 139",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/1000",
            "value": 1523,
            "range": "± 235",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/2000",
            "value": 3174,
            "range": "± 276",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/4000",
            "value": 6182,
            "range": "± 377",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/GET",
            "value": 78,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/POST",
            "value": 80,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PUT",
            "value": 78,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/DELETE",
            "value": 86,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PATCH",
            "value": 83,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/HEAD",
            "value": 81,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/OPTIONS",
            "value": 90,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/0",
            "value": 67,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/1",
            "value": 81,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/2",
            "value": 93,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/3",
            "value": 72,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/4",
            "value": 79,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation",
            "value": 80,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation_test",
            "value": 81,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/100",
            "value": 37,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/1000",
            "value": 38,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/10000",
            "value": 44,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_simple",
            "value": 25,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_complex",
            "value": 21,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "tcp_route_lookup",
            "value": 1,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "backend_selection_round_robin",
            "value": 1,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_table_creation",
            "value": 189,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "backend_creation",
            "value": 41,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "add_http_route",
            "value": 579,
            "range": "± 49",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}