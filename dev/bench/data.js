window.BENCHMARK_DATA = {
  "lastUpdate": 1792595086955,
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
      },
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
          "id": "95a07bd6b4d54dff33de4fe7828e370cda9e07f7",
          "message": "Bench gate: enforce locally on owned hardware, CI advisory only\n\nShared-runner variance swamps any honest threshold (a docs-only PR\nmeasured a uniform 1.5-1.6x from hardware alone), so no threshold can be\nboth strict and non-flaky there. The CI workflow keeps tracking history\nand commenting at 150% but never fails a PR. The enforcing gate is\nscripts/bench-gate.sh (make bench-baseline / bench-check): 1.15x against\na baseline recorded on the same machine, with a 2 ns absolute floor\nbecause bencher output is integer ns and the sub-ns benches print 0-1.\nVerified: back-to-back self-check passes (worst drift 1.08x), a halved\nbaseline fails exit 1 naming each bench. Reverts the 200% threshold.",
          "timestamp": "2026-07-18T21:19:43+02:00",
          "tree_id": "955128389aedb30ea8800cf93dbcc145dddd0516",
          "url": "https://github.com/epheo/portail/commit/95a07bd6b4d54dff33de4fe7828e370cda9e07f7"
        },
        "date": 1784403485392,
        "tool": "cargo",
        "benches": [
          {
            "name": "e2e_http_request_processing",
            "value": 892,
            "range": "± 156",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_auth_request",
            "value": 170,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_product_search",
            "value": 274,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_health_check_processing",
            "value": 103,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_404_route_processing",
            "value": 239,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "throughput_simulation_1000_requests",
            "value": 500263,
            "range": "± 14742",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_simple_get",
            "value": 116,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_auth",
            "value": 178,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_cookies",
            "value": 157,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_complex_path",
            "value": 286,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple",
            "value": 67,
            "range": "± 18",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical",
            "value": 193,
            "range": "± 11",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex",
            "value": 667,
            "range": "± 35",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple_repeated",
            "value": 67,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical_repeated",
            "value": 211,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex_repeated",
            "value": 660,
            "range": "± 20",
            "unit": "ns/iter"
          },
          {
            "name": "parse_malformed_request",
            "value": 89,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/100",
            "value": 122,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/500",
            "value": 887,
            "range": "± 31",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/1000",
            "value": 1681,
            "range": "± 82",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/2000",
            "value": 3388,
            "range": "± 88",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/4000",
            "value": 6815,
            "range": "± 206",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/GET",
            "value": 81,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/POST",
            "value": 83,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PUT",
            "value": 81,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/DELETE",
            "value": 94,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PATCH",
            "value": 85,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/HEAD",
            "value": 83,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/OPTIONS",
            "value": 90,
            "range": "± 3",
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
            "value": 83,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/2",
            "value": 98,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/3",
            "value": 76,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/4",
            "value": 86,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation",
            "value": 96,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation_test",
            "value": 96,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/100",
            "value": 60,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/1000",
            "value": 61,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/10000",
            "value": 63,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_simple",
            "value": 35,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_complex",
            "value": 32,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "tcp_route_lookup",
            "value": 2,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "backend_selection_round_robin",
            "value": 2,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_table_creation",
            "value": 171,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "backend_creation",
            "value": 52,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "add_http_route",
            "value": 662,
            "range": "± 125",
            "unit": "ns/iter"
          }
        ]
      },
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
          "id": "e58261a438a738917806a2b980d289cb44ffc32a",
          "message": "Opt-in access logs: one JSON line per completed HTTP response\n\n--access-log <PATH> (\"-\" = stdout), off by default; disabled costs the\nhot path one relaxed load per request. Request fields are copied out at\nparse time (the buffer is reused for the response phase), the line goes\nto a dedicated writer through a bounded channel, and a sink slower than\nthe request rate sheds lines into portail_access_log_dropped_total\nrather than slowing the data path. Response status is recorded on\nConnectionState.last_status at each emit site (fail helpers included);\nforward_response_headers now returns the error status it wrote instead\nof an anonymous None. Attacker-controlled fields (method, path, Host)\nare JSON-escaped. Throughput held at 334-356k req/s.",
          "timestamp": "2026-07-19T22:28:25+02:00",
          "tree_id": "0a5d9dfe9e580036c4627f84f4a99fab5380e658",
          "url": "https://github.com/epheo/portail/commit/e58261a438a738917806a2b980d289cb44ffc32a"
        },
        "date": 1784494026373,
        "tool": "cargo",
        "benches": [
          {
            "name": "e2e_http_request_processing",
            "value": 912,
            "range": "± 24",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_auth_request",
            "value": 173,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_product_search",
            "value": 268,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_health_check_processing",
            "value": 98,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_404_route_processing",
            "value": 235,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "throughput_simulation_1000_requests",
            "value": 485706,
            "range": "± 16474",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_simple_get",
            "value": 112,
            "range": "± 11",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_auth",
            "value": 174,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_cookies",
            "value": 163,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_complex_path",
            "value": 286,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple",
            "value": 69,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical",
            "value": 195,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex",
            "value": 663,
            "range": "± 23",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple_repeated",
            "value": 69,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical_repeated",
            "value": 195,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex_repeated",
            "value": 664,
            "range": "± 28",
            "unit": "ns/iter"
          },
          {
            "name": "parse_malformed_request",
            "value": 92,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/100",
            "value": 123,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/500",
            "value": 887,
            "range": "± 27",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/1000",
            "value": 1677,
            "range": "± 73",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/2000",
            "value": 3389,
            "range": "± 229",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/4000",
            "value": 6819,
            "range": "± 672",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/GET",
            "value": 82,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/POST",
            "value": 89,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PUT",
            "value": 82,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/DELETE",
            "value": 90,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PATCH",
            "value": 85,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/HEAD",
            "value": 83,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/OPTIONS",
            "value": 91,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/0",
            "value": 68,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/1",
            "value": 81,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/2",
            "value": 99,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/3",
            "value": 76,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/4",
            "value": 83,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation",
            "value": 99,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation_test",
            "value": 99,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/100",
            "value": 60,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/1000",
            "value": 61,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/10000",
            "value": 63,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_simple",
            "value": 35,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_complex",
            "value": 32,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "tcp_route_lookup",
            "value": 2,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "backend_selection_round_robin",
            "value": 2,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_table_creation",
            "value": 170,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "backend_creation",
            "value": 53,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "add_http_route",
            "value": 656,
            "range": "± 161",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "root@epheo.eu",
            "name": "Thibaut Lapierre",
            "username": "epheo"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "5890130954be00dd76484409b15cd13365c35acc",
          "message": "Per-client rate limiting via RateLimitPolicy CRD (#12)",
          "timestamp": "2026-07-28T15:08:27+02:00",
          "tree_id": "d0ad40530d0b529445a186d71d0997499b7a05d5",
          "url": "https://github.com/epheo/portail/commit/5890130954be00dd76484409b15cd13365c35acc"
        },
        "date": 1785245382907,
        "tool": "cargo",
        "benches": [
          {
            "name": "e2e_http_request_processing",
            "value": 882,
            "range": "± 68",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_auth_request",
            "value": 173,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_product_search",
            "value": 256,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_health_check_processing",
            "value": 98,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_404_route_processing",
            "value": 255,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "throughput_simulation_1000_requests",
            "value": 481708,
            "range": "± 14898",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_simple_get",
            "value": 110,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_auth",
            "value": 174,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_cookies",
            "value": 156,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_complex_path",
            "value": 241,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple",
            "value": 66,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical",
            "value": 209,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex",
            "value": 687,
            "range": "± 16",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple_repeated",
            "value": 66,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical_repeated",
            "value": 209,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex_repeated",
            "value": 687,
            "range": "± 18",
            "unit": "ns/iter"
          },
          {
            "name": "parse_malformed_request",
            "value": 89,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/100",
            "value": 122,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/500",
            "value": 765,
            "range": "± 29",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/1000",
            "value": 1412,
            "range": "± 42",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/2000",
            "value": 2960,
            "range": "± 186",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/4000",
            "value": 5684,
            "range": "± 251",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/GET",
            "value": 84,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/POST",
            "value": 86,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PUT",
            "value": 84,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/DELETE",
            "value": 93,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PATCH",
            "value": 89,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/HEAD",
            "value": 86,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/OPTIONS",
            "value": 95,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/0",
            "value": 70,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/1",
            "value": 82,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/2",
            "value": 92,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/3",
            "value": 78,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/4",
            "value": 84,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation",
            "value": 96,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation_test",
            "value": 96,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/100",
            "value": 63,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/1000",
            "value": 66,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/10000",
            "value": 67,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_simple",
            "value": 35,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_complex",
            "value": 31,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "tcp_route_lookup",
            "value": 2,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "backend_selection_round_robin",
            "value": 2,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_table_creation",
            "value": 184,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "backend_creation",
            "value": 53,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "add_http_route",
            "value": 711,
            "range": "± 139",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "root@epheo.eu",
            "name": "Thibaut Lapierre",
            "username": "epheo"
          },
          "committer": {
            "email": "root@epheo.eu",
            "name": "Thibaut Lapierre",
            "username": "epheo"
          },
          "distinct": true,
          "id": "99a1630ac5021f4a03e088c13aec2ed41d8f5a10",
          "message": "Request sniff: accept every HTTP method, not just the seven common ones",
          "timestamp": "2026-09-15T21:47:57+02:00",
          "tree_id": "63b3f88d134f57918607d588339343b0603ecbcc",
          "url": "https://github.com/epheo/portail/commit/99a1630ac5021f4a03e088c13aec2ed41d8f5a10"
        },
        "date": 1789502939646,
        "tool": "cargo",
        "benches": [
          {
            "name": "e2e_http_request_processing",
            "value": 671,
            "range": "± 13",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_auth_request",
            "value": 140,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_product_search",
            "value": 197,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_health_check_processing",
            "value": 81,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_404_route_processing",
            "value": 188,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "throughput_simulation_1000_requests",
            "value": 373995,
            "range": "± 8684",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_simple_get",
            "value": 90,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_auth",
            "value": 139,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_cookies",
            "value": 127,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_complex_path",
            "value": 222,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple",
            "value": 56,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical",
            "value": 185,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex",
            "value": 496,
            "range": "± 18",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple_repeated",
            "value": 56,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical_repeated",
            "value": 184,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex_repeated",
            "value": 499,
            "range": "± 27",
            "unit": "ns/iter"
          },
          {
            "name": "parse_malformed_request",
            "value": 71,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/100",
            "value": 107,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/500",
            "value": 667,
            "range": "± 20",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/1000",
            "value": 1345,
            "range": "± 34",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/2000",
            "value": 2710,
            "range": "± 55",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/4000",
            "value": 5445,
            "range": "± 266",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/GET",
            "value": 68,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/POST",
            "value": 70,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PUT",
            "value": 69,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/DELETE",
            "value": 76,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PATCH",
            "value": 72,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/HEAD",
            "value": 70,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/OPTIONS",
            "value": 78,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/0",
            "value": 58,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/1",
            "value": 69,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/2",
            "value": 90,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/3",
            "value": 65,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/4",
            "value": 70,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation",
            "value": 78,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation_test",
            "value": 78,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/100",
            "value": 47,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/1000",
            "value": 48,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/10000",
            "value": 49,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_simple",
            "value": 29,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_complex",
            "value": 24,
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
            "value": 156,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "backend_creation",
            "value": 43,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "add_http_route",
            "value": 449,
            "range": "± 74",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_common_method",
            "value": 2,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_extension_method",
            "value": 14,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_non_http",
            "value": 1,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "root@epheo.eu",
            "name": "Thibaut Lapierre",
            "username": "epheo"
          },
          "committer": {
            "email": "root@epheo.eu",
            "name": "Thibaut Lapierre",
            "username": "epheo"
          },
          "distinct": true,
          "id": "2b236fa2cb055dc956d938e3c47cc9dfbf7b3a8f",
          "message": "Health: never abandon a parked backend; probe with capped backoff\n\nA backend marked unhealthy receives no traffic, so the \"future passive\nfailure\" that was supposed to restart probing after 30 attempts can never\nhappen. A Service that was down longer than five minutes stayed at 503 until\nthe process restarted, even after it came back. def.ms hit this after five\ndays without endpoints.\n\nThe probe now backs off from 10s to 5min and keeps going until the backend\nanswers or clear() drops the entry.\n\nCo-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>",
          "timestamp": "2026-10-04T23:05:31+02:00",
          "tree_id": "97b66510f848ef47546b985c3dcc7afabb7cf371",
          "url": "https://github.com/epheo/portail/commit/2b236fa2cb055dc956d938e3c47cc9dfbf7b3a8f"
        },
        "date": 1791149850973,
        "tool": "cargo",
        "benches": [
          {
            "name": "e2e_http_request_processing",
            "value": 879,
            "range": "± 46",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_auth_request",
            "value": 169,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_product_search",
            "value": 235,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_health_check_processing",
            "value": 98,
            "range": "± 11",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_404_route_processing",
            "value": 236,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "throughput_simulation_1000_requests",
            "value": 493847,
            "range": "± 16768",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_simple_get",
            "value": 99,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_auth",
            "value": 167,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_cookies",
            "value": 144,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_complex_path",
            "value": 274,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple",
            "value": 64,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical",
            "value": 248,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex",
            "value": 694,
            "range": "± 23",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple_repeated",
            "value": 64,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical_repeated",
            "value": 247,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex_repeated",
            "value": 694,
            "range": "± 22",
            "unit": "ns/iter"
          },
          {
            "name": "parse_malformed_request",
            "value": 97,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/100",
            "value": 128,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/500",
            "value": 759,
            "range": "± 32",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/1000",
            "value": 1533,
            "range": "± 80",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/2000",
            "value": 2802,
            "range": "± 101",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/4000",
            "value": 5625,
            "range": "± 341",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/GET",
            "value": 88,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/POST",
            "value": 90,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PUT",
            "value": 87,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/DELETE",
            "value": 96,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PATCH",
            "value": 92,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/HEAD",
            "value": 90,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/OPTIONS",
            "value": 99,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/0",
            "value": 67,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/1",
            "value": 80,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/2",
            "value": 99,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/3",
            "value": 76,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/4",
            "value": 82,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation",
            "value": 94,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation_test",
            "value": 94,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/100",
            "value": 59,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/1000",
            "value": 61,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/10000",
            "value": 63,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_simple",
            "value": 36,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_complex",
            "value": 32,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "tcp_route_lookup",
            "value": 2,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "backend_selection_round_robin",
            "value": 2,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_table_creation",
            "value": 170,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "backend_creation",
            "value": 49,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "add_http_route",
            "value": 554,
            "range": "± 87",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_common_method",
            "value": 2,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_extension_method",
            "value": 17,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_non_http",
            "value": 1,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      },
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
          "id": "f8e5ba9f898e51fde27c33069b40edac8ec162c4",
          "message": "Empty lines: before the first request too, and across reads\n\nEmpty lines were only skipped on keepalive iterations, and a read ending\ninside one (a lone CR) closed the connection.\n\n- CRLF pairs opening a connection are dropped when an HTTP request-line\n  follows. Otherwise the bytes stay as sent, for TCP routing.\n- A read that ends inside the empty lines is extended before deciding,\n  within client_header_timeout like a request head. Between keepalive\n  requests, complete empty lines still wait as long as the idle wait.",
          "timestamp": "2026-10-21T03:10:56+02:00",
          "tree_id": "8db63772a32f0f00ecda76852f1654b555e16c22",
          "url": "https://github.com/epheo/portail/commit/f8e5ba9f898e51fde27c33069b40edac8ec162c4"
        },
        "date": 1792546253470,
        "tool": "cargo",
        "benches": [
          {
            "name": "e2e_http_request_processing",
            "value": 323,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_auth_request",
            "value": 76,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_product_search",
            "value": 67,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_health_check_processing",
            "value": 31,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_404_route_processing",
            "value": 97,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "throughput_simulation_1000_requests",
            "value": 189603,
            "range": "± 20343",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_simple_get",
            "value": 49,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_auth",
            "value": 62,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_cookies",
            "value": 59,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_complex_path",
            "value": 67,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple",
            "value": 25,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical",
            "value": 55,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex",
            "value": 146,
            "range": "± 28",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple_repeated",
            "value": 26,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical_repeated",
            "value": 55,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex_repeated",
            "value": 146,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_malformed_request",
            "value": 31,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/100",
            "value": 36,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/500",
            "value": 89,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/1000",
            "value": 159,
            "range": "± 15",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/2000",
            "value": 307,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/4000",
            "value": 617,
            "range": "± 13",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/GET",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/POST",
            "value": 30,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PUT",
            "value": 30,
            "range": "± 14",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/DELETE",
            "value": 30,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PATCH",
            "value": 30,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/HEAD",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/OPTIONS",
            "value": 31,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/0",
            "value": 28,
            "range": "± 16",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/1",
            "value": 31,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/2",
            "value": 35,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/3",
            "value": 32,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/4",
            "value": 34,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation",
            "value": 33,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation_test",
            "value": 34,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/100",
            "value": 31,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/1000",
            "value": 31,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/10000",
            "value": 32,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_simple",
            "value": 19,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_complex",
            "value": 17,
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
            "value": 156,
            "range": "± 13",
            "unit": "ns/iter"
          },
          {
            "name": "backend_creation",
            "value": 28,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "add_http_route",
            "value": 595,
            "range": "± 161",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_common_method",
            "value": 1,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_extension_method",
            "value": 8,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_non_http",
            "value": 0,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "github@epheo.eu",
            "name": "Thibaut Lapierre",
            "username": "epheo"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "86a9ca51fa64a5fe6293130d93779cfaeb830612",
          "message": "Control-plane correctness: one attachment truth, one validation funnel, exact status, shared TLS ports (#17)\n\nControl-plane correctness, wave 2 of the v0.1.22 audit. Builds on\nv0.1.26.\nThe data plane now serves exactly what status says, every route is\nchecked once by one funnel, and status is exact and cheap.\n\n## Attachment has one source of truth\n- The reconciler emits the listeners each parentRef was accepted by; the\ntable no longer re-derives attachment from raw parentRefs. allowedRoutes\nnamespaces and kinds were bypassed before, and a ref to another\nnamespace's Gateway of the same name attached.\n- A parentRef without `namespace` means the route's own namespace.\n- TCP/UDP routes attach like HTTP routes: bare and port-only refs and\nevery parentRef work; the oldest route owns an L4 listener, later ones\nreport Conflicted.\n- An invalid backendRef keeps its weight and fails its share (HTTP 500,\nL4 close), per HTTPBackendRef.\n- Equally specific routes tie-break by creationTimestamp, then\n\"{namespace}/{name}\".\n- Conflicted listeners are not served; HTTPS and TLS passthrough share a\nport when their hostnames differ.\n\n## One validation funnel\n- One step checks and compiles each HTTP route (config/compile.rs) for\nfiles and Kubernetes alike; the table build consumes it, so each regex\ncompiles once.\n- One bad route is rejected alone (Accepted=False, UnsupportedValue or\nIncompatibleFilters); the rest of the table goes live. Before, it failed\nthe whole Gateway's build.\n- Regexes match the whole path, header or query value.\n- Mirror fractions are exact; a rule without matches matches every path;\nPathPrefix ignores a trailing slash.\n- Route checks are cached per route generation; a table build sorts once\n(4000 rules: 28 ms to 1 ms).\n\n## Request time\n- Dot-segments are removed before matching (RFC 3986 5.2.4, %2e counts\nas a dot), and the backend gets that path: /x/../admin no longer slips\npast the rule guarding /admin. A dot-segment with path parameters\n(/..;/) gets a 400.\n- URLRewrite and redirects keep the query string; a value's own query\njoins the request's with '&'.\n- 429s count only as rate-limited, not as dispatched; an h2 refused\nstream and a malformed chunked body count as rejected.\n- Route misses allocate nothing; L4 route misses log once per window.\n- Every route kind and mirrors reach headless Services on their\ntargetPort and ExternalName Services at all.\n\n## Controller status\n- Status is built without timestamps and written only where it differs\nfrom the store; spec bumps, wipes and recreations are healed.\n- A route or policy that moves away loses our status entry on the old\nGateway.\n- Listener Programmed follows the real bind; attachedRoutes counts\nattached routes that are not accepted.\n- portail.epheo.eu/Network addresses bind their resolved IP (they never\nbound before, so the wildcard served every network). A failed NAD read\nkeeps the last IP; listeners move between the wildcard and a specific\naddress in one pass.\n- API load is scoped to the backends this Gateway routes to; named\ntargetPorts resolve by Service port name and are cached; optional CRDs\ninstalled later get their watch without a restart.\n- Config and route table publish as one pair; reflector caches drop\nmanagedFields.\n- RBAC examples: status verbs are patch only, gatewayclasses/status\ndropped, NAD get added.\n\n## Shared TLS ports\n- Each TLS port serves by a plan published per pass and read per\nconnection: mode and cert changes need no rebind, and a deleted\nlistener's cert stops being served.\n- SNI selects a listener by precedence across HTTPS and TLS (exact,\nwildcard, catch-all); no SNI goes to the catch-all.\n- TLS routes match only within their listener. A bad cert rotation keeps\nthe last good certs; a listener whose own certs failed refuses its SNI.\n- A TLS listener in Terminate mode is rejected (UnsupportedProtocol):\nTLSRoute is served as passthrough only.\n\n## Behavior changes\n- Routes may stop being served where their status already said they were\nnot attached (allowedRoutes, other-namespace refs, conflicted\nlisteners).\n- An invalid backendRef's share fails instead of moving to the valid\nbackends.\n- Regexes are full matches; paths with dot-segments route and forward\nnormalized; /..;/ is a 400.\n- File configs: conflicting protocols on one port, a mirror with both\npercent and fraction, and repeated filters fail to load; a redirect\nbeside backendRefs loads with a warning.\n- portail_http_requests_total no longer counts 429s.\n- Network addresses now bind; an unresolved one binds nothing (never the\nwildcard).\n- Status writes are fewer and exact; reconcile_runs_total means rebuilt\nor wrote status.\n\n## Evidence\n- Tests: lib 607 (472 on v0.1.26); integration: parser 69, proxy 45, h2\n24, tls 3, udp 5, hot-path allocations 7. Every commit builds.\n- Replay of the 15 smuggling scenarios: unchanged, all closed.\n- cargo deny: ok.\n- Throughput A/B vs v0.1.26, 3 interleaved rounds: neutral (no-filter 1\nbackend 469.6k vs 468.7k req/s, 4 backends 456.1k vs 455.2k, filter\n465.3k vs 463.5k); p50 and p99 unchanged. Churn neutral.\n- Instructions per request: +1.8% (path normalization and the added\nchecks); cycles within 0.5%.\n- Criterion: routing benches were rewritten with opaque inputs (old\nbodies let the compiler hoist lookups), so their numbers moved with the\nbodies; parse benches flagged by the gate are a layout artifact of the\nbench binary (rebuilt without the added bench group they match v0.1.26).\n- Peer review: /code-review xhigh on the integrated wave, then high and\nthree medium rounds on the fix packages; every finding triaged.\n\n## Deferred\n- A cert made unusable by a revoked ReferenceGrant keeps serving when\nevery cert on its port fails (as today); fixed by the controller split,\nwhich sends certs explicitly.\n- TCP and UDP listeners on one (address, port) cannot both bind yet.\n- Legacy unscoped mode still merges Gateways by port.",
          "timestamp": "2026-10-21T16:10:33+02:00",
          "tree_id": "e5178564695808cc92c07d3559cb32cea74355a4",
          "url": "https://github.com/epheo/portail/commit/86a9ca51fa64a5fe6293130d93779cfaeb830612"
        },
        "date": 1792593315853,
        "tool": "cargo",
        "benches": [
          {
            "name": "e2e_http_request_processing",
            "value": 346,
            "range": "± 15",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_auth_request",
            "value": 65,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_product_search",
            "value": 65,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_health_check_processing",
            "value": 29,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_404_route_processing",
            "value": 46,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "throughput_simulation_1000_requests",
            "value": 196914,
            "range": "± 11223",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_simple_get",
            "value": 50,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_auth",
            "value": 64,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_cookies",
            "value": 60,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_complex_path",
            "value": 63,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_analyze_request_weighted_backends",
            "value": 78,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_analyze_path_clean",
            "value": 87,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_analyze_path_dotted",
            "value": 93,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_analyze_path_dot_segments",
            "value": 134,
            "range": "± 14",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_analyze_redirect_prefix_with_query",
            "value": 182,
            "range": "± 17",
            "unit": "ns/iter"
          },
          {
            "name": "url_rewrite_prefix",
            "value": 28,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "url_rewrite_full_path",
            "value": 10,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_analyze_redirect_full_value_query",
            "value": 217,
            "range": "± 8",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple",
            "value": 28,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical",
            "value": 56,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex",
            "value": 143,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple_repeated",
            "value": 27,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical_repeated",
            "value": 55,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex_repeated",
            "value": 143,
            "range": "± 13",
            "unit": "ns/iter"
          },
          {
            "name": "parse_malformed_request",
            "value": 32,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/100",
            "value": 35,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/500",
            "value": 87,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/1000",
            "value": 165,
            "range": "± 13",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/2000",
            "value": 309,
            "range": "± 25",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/4000",
            "value": 602,
            "range": "± 11",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/GET",
            "value": 28,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/POST",
            "value": 33,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PUT",
            "value": 28,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/DELETE",
            "value": 29,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PATCH",
            "value": 29,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/HEAD",
            "value": 28,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/OPTIONS",
            "value": 30,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/0",
            "value": 26,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/1",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/2",
            "value": 39,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/3",
            "value": 30,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/4",
            "value": 31,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "normalize_path/clean",
            "value": 2,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "normalize_path/dotted",
            "value": 6,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "normalize_path/dot_segments",
            "value": 46,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "normalize_path/dot_segment_params",
            "value": 3,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation",
            "value": 38,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation_test",
            "value": 38,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/100",
            "value": 28,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/1000",
            "value": 28,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/10000",
            "value": 30,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "route_miss/unknown_host",
            "value": 28,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "route_miss/unrouted_path",
            "value": 41,
            "range": "± 9",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_simple",
            "value": 21,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_complex",
            "value": 18,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "tcp_route_lookup",
            "value": 2,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "tls_sni_listener_select",
            "value": 34,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "tls_route_resolve_wildcard",
            "value": 62,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "udp_route_miss",
            "value": 27,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "backend_selection/l4_round_robin_100",
            "value": 69,
            "range": "± 14",
            "unit": "ns/iter"
          },
          {
            "name": "backend_selection/http_weighted_4",
            "value": 6,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_table_creation",
            "value": 137,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "backend_creation",
            "value": 29,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "add_http_route",
            "value": 379,
            "range": "± 89",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_regex_hit",
            "value": 43,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_regex_miss",
            "value": 33,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "http_route_lookup_shared_scope",
            "value": 20,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "http_route_lookup_own_and_shared",
            "value": 35,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_table_build_1000_rules",
            "value": 915850,
            "range": "± 46170",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_common_method",
            "value": 1,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_extension_method",
            "value": 7,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_non_http",
            "value": 0,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      },
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
          "id": "2cd5061b8e70d0fc06a13f6bbafd8c8a52094a06",
          "message": "Release: a vX.Y.Z tag publishes a release; main publishes only its image\n\nEvery push to main used to bump the patch version, commit it to main, tag and release.\nThat commit landed after each merge, so a branch rebased before it could no longer be rebase-merged.\nNow CI never commits to main:\n- a push to main runs CI and conformance, then pushes the :main and :sha-<commit> images;\n- a vX.Y.Z tag runs the same gates, checks Cargo.toml and the conformance harness already say X.Y.Z, then pushes :X.Y.Z, :X.Y and :latest and creates the GitHub release.\nscripts/release.sh X.Y.Z sets those versions for the release commit.\n:latest now means the last release, not the last commit on main.",
          "timestamp": "2026-10-21T16:36:11+02:00",
          "tree_id": "8a32672057ec8d6ec51c5474223bc81ae4afe386",
          "url": "https://github.com/epheo/portail/commit/2cd5061b8e70d0fc06a13f6bbafd8c8a52094a06"
        },
        "date": 1792595086288,
        "tool": "cargo",
        "benches": [
          {
            "name": "e2e_http_request_processing",
            "value": 511,
            "range": "± 12",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_auth_request",
            "value": 116,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_single_product_search",
            "value": 106,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_health_check_processing",
            "value": 54,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_404_route_processing",
            "value": 89,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "throughput_simulation_1000_requests",
            "value": 279783,
            "range": "± 7050",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_simple_get",
            "value": 83,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_auth",
            "value": 105,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_with_cookies",
            "value": 97,
            "range": "± 10",
            "unit": "ns/iter"
          },
          {
            "name": "latency_scenario_complex_path",
            "value": 104,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_analyze_request_weighted_backends",
            "value": 129,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_analyze_path_clean",
            "value": 130,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_analyze_path_dotted",
            "value": 136,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_analyze_path_dot_segments",
            "value": 219,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_analyze_redirect_prefix_with_query",
            "value": 256,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "url_rewrite_prefix",
            "value": 36,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "url_rewrite_full_path",
            "value": 14,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "e2e_analyze_redirect_full_value_query",
            "value": 290,
            "range": "± 12",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple",
            "value": 45,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical",
            "value": 96,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex",
            "value": 229,
            "range": "± 6",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_simple_repeated",
            "value": 45,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_typical_repeated",
            "value": 95,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_complex_repeated",
            "value": 230,
            "range": "± 13",
            "unit": "ns/iter"
          },
          {
            "name": "parse_malformed_request",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/100",
            "value": 66,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/500",
            "value": 127,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/1000",
            "value": 202,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/2000",
            "value": 376,
            "range": "± 15",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_request_size/parse_headers_fast/4000",
            "value": 675,
            "range": "± 12",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/GET",
            "value": 54,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/POST",
            "value": 56,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PUT",
            "value": 54,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/DELETE",
            "value": 53,
            "range": "± 5",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/PATCH",
            "value": 54,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/HEAD",
            "value": 56,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_method/parse_headers_fast/OPTIONS",
            "value": 54,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/0",
            "value": 47,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/1",
            "value": 54,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/2",
            "value": 64,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/3",
            "value": 57,
            "range": "± 7",
            "unit": "ns/iter"
          },
          {
            "name": "parse_by_host_complexity/parse_headers_fast/4",
            "value": 56,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "normalize_path/clean",
            "value": 3,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "normalize_path/dotted",
            "value": 12,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "normalize_path/dot_segments",
            "value": 84,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "normalize_path/dot_segment_params",
            "value": 5,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation",
            "value": 67,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "parse_headers_fast_allocation_test",
            "value": 67,
            "range": "± 4",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/100",
            "value": 34,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/1000",
            "value": 35,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_lookup_by_table_size/http_route_lookup/10000",
            "value": 37,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "route_miss/unknown_host",
            "value": 48,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "route_miss/unrouted_path",
            "value": 51,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_simple",
            "value": 34,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_complex",
            "value": 29,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "tcp_route_lookup",
            "value": 2,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "tls_sni_listener_select",
            "value": 44,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "tls_route_resolve_wildcard",
            "value": 77,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "udp_route_miss",
            "value": 27,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "backend_selection/l4_round_robin_100",
            "value": 105,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "backend_selection/http_weighted_4",
            "value": 16,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "route_table_creation",
            "value": 176,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "backend_creation",
            "value": 38,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "add_http_route",
            "value": 512,
            "range": "± 31",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_regex_hit",
            "value": 51,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "path_matching_regex_miss",
            "value": 50,
            "range": "± 2",
            "unit": "ns/iter"
          },
          {
            "name": "http_route_lookup_shared_scope",
            "value": 30,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "http_route_lookup_own_and_shared",
            "value": 54,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "route_table_build_1000_rules",
            "value": 1144742,
            "range": "± 19639",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_common_method",
            "value": 0,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_extension_method",
            "value": 11,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "sniff_non_http",
            "value": 1,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}