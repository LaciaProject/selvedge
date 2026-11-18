#!/usr/bin/env python3
"""Writes the Grafana dashboard for Selvedge's Prometheus metrics (docs/metrics.md).

    tools/grafana-dashboard.py > deploy/helm/selvedge/dashboards/selvedge.json

Grafana's JSON is long and repetitive; this keeps the panels readable. Edit
here, not in the JSON. With --rules, it prints the queries as a Prometheus
rules file instead, for `promtool check rules` (CI checks they parse).
"""

import json
import sys

DS = {"type": "prometheus", "uid": "${datasource}"}
SEL = 'namespace=~"$namespace"'


class Layout:
    def __init__(self):
        self.panels, self.y, self.x, self.row_h, self.next_id = [], 0, 0, 0, 1

    def _id(self):
        self.next_id += 1
        return self.next_id - 1

    def row(self, title):
        if self.x:
            self.y += self.row_h
        self.x, self.row_h = 0, 0
        self.panels.append({"type": "row", "title": title, "id": self._id(), "collapsed": False,
                            "gridPos": {"h": 1, "w": 24, "x": 0, "y": self.y}, "panels": []})
        self.y += 1

    def panel(self, title, targets, *, kind="timeseries", unit="short", w=8, h=8, desc="", stack=False):
        if self.x + w > 24:
            self.y += self.row_h
            self.x, self.row_h = 0, 0
        p = {
            "type": kind, "title": title, "id": self._id(), "datasource": DS, "description": desc,
            "gridPos": {"h": h, "w": w, "x": self.x, "y": self.y},
            "targets": [{"datasource": DS, "expr": e, "legendFormat": l, "refId": chr(65 + i)}
                        for i, (e, l) in enumerate(targets)],
            "fieldConfig": {"defaults": {"unit": unit}, "overrides": []},
            "options": {"legend": {"displayMode": "list", "placement": "bottom"}},
        }
        if kind == "timeseries":
            p["fieldConfig"]["defaults"]["custom"] = {
                "lineWidth": 1, "fillOpacity": 10, "showPoints": "never",
                "stacking": {"mode": "normal" if stack else "none"},
            }
        if kind == "stat":
            p["options"] = {"reduceOptions": {"calcs": ["lastNotNull"]}, "colorMode": "value", "graphMode": "area"}
        self.panels.append(p)
        self.x += w
        self.row_h = max(self.row_h, h)


def quantiles(metric, by="", extra=""):
    grp = f"le{', ' + by if by else ''}"
    leg = "{{" + by + "}} " if by else ""
    return [(f"histogram_quantile({q}, sum by ({grp}) (rate({metric}_bucket{{{SEL}{extra}}}[$__rate_interval])))",
             f"{leg}p{int(q * 100)}") for q in (0.5, 0.95, 0.99)]


def rate(metric, by, extra=""):
    return f"sum by ({by}) (rate({metric}{{{SEL}{extra}}}[$__rate_interval]))"


d = Layout()

d.row("Traffic")
d.panel("Requests", [(rate("selvedge_http_requests_total", "component"), "{{component}}")], unit="reqps",
        desc="Requests served by each component, health checks excluded.")
d.panel("Error ratio (5xx)", [(
    f'sum by (component) (rate(selvedge_http_requests_total{{{SEL}, code="5xx"}}[$__rate_interval]))'
    f" / sum by (component) (rate(selvedge_http_requests_total{{{SEL}}}[$__rate_interval]))",
    "{{component}}")], unit="percentunit")
d.panel("Responses by class", [(rate("selvedge_http_requests_total", "code", ', component="gateway"'), "{{code}}")],
        unit="reqps", stack=True, desc="The gateway's responses, which is what clients of Workers get.")
d.panel("Gateway latency", quantiles("selvedge_http_request_duration_seconds", extra=', component="gateway"'),
        unit="s", w=12, desc="To the response headers, including the Worker's own time.")
d.panel("API latency", quantiles("selvedge_http_request_duration_seconds", extra=', component="api"'),
        unit="s", w=12)

d.row("Gateway and runners")
d.panel("Healthy runners", [(f"min(selvedge_gateway_healthy_runners{{{SEL}}})", "fewest any gateway sees")],
        kind="stat", w=6)
d.panel("Account processes", [(f"sum(selvedge_agent_account_processes{{{SEL}}})", "running")], kind="stat", w=6)
d.panel("Runner errors at the gateway",
        [(rate("selvedge_gateway_runner_errors_total", "kind"), "{{kind}}")], unit="ops", w=12,
        desc="unreachable: tried the next runner. failed: the runner took the request and failed it (502).")
d.panel("Account processes per runner", [(f"sum by (pod) (selvedge_agent_account_processes{{{SEL}}})", "{{pod}}")],
        w=12)
d.panel("Process starts and exits", [
    (f"sum(rate(selvedge_agent_process_starts_total{{{SEL}}}[$__rate_interval]))", "starts"),
    (f"sum(rate(selvedge_agent_process_exits_total{{{SEL}}}[$__rate_interval]))", "exited on their own"),
], unit="ops", w=12, desc="Exits are crashes, or OOM kills in cgroup mode.")
d.panel("Process stops by reason", [(rate("selvedge_agent_process_stops_total", "reason"), "{{reason}}")],
        unit="ops", stack=True, w=12, desc="memory and hung are limits enforced by the agent.")
d.panel("Process start time", quantiles("selvedge_agent_process_start_duration_seconds"), unit="s", w=12,
        desc="From fetching an account's bundle to its sockets accepting.")
d.panel("Durable Objects and CPU", [
    (f"sum(rate(selvedge_agent_cluster_leases_given_up_total{{{SEL}}}[$__rate_interval]))", "leases given up"),
    (f"sum(rate(selvedge_agent_cpu_throttles_total{{{SEL}}}[$__rate_interval]))", "CPU throttles"),
], unit="ops", w=12, desc="A lease given up moves a dead process's Durable Objects at once.")

d.row("Bindings and connections")
d.panel("Binding calls", [(rate("selvedge_agent_binding_requests_total", "binding"), "{{binding}}")], unit="ops",
        stack=True, w=12)
d.panel("Failed binding calls", [(rate("selvedge_agent_binding_requests_total", "binding, code",
                                       ', code!~"2..|3..|404"'), "{{binding}} {{code}}")], unit="ops", w=12,
        desc="Calls to the data service that failed or answered with an error.")
d.panel("Connections opened", [(rate("selvedge_connections_opened_total", "role"), "to {{role}}")], unit="ops",
        w=24, desc="Connections are kept and reused: this should stay far below the request rate.")

d.row("Containers")
d.panel("Instances", [(f"sum by (state) (selvedge_container_instances{{{SEL}}})", "{{state}}")], w=8, stack=True)
d.panel("Starts", [(rate("selvedge_container_starts_total", "result"), "{{result}}")], unit="ops", w=8,
        desc="limit: refused by max_instances or an account or installation limit.")
d.panel("Stops by reason", [(rate("selvedge_container_stops_total", "reason"), "{{reason}}")], unit="ops", w=8,
        stack=True)
d.panel("Start time", quantiles("selvedge_agent_container_start_duration_seconds", extra=', result="ok"'),
        unit="s", w=12, desc="Scheduling, image pull and sandbox boot.")
d.panel("Unsupported and refused", [
    (rate("selvedge_agent_container_unsupported_total", "operation"), "unsupported {{operation}}"),
    (f"sum(rate(selvedge_agent_container_egress_refused_total{{{SEL}}}[$__rate_interval]))", "egress refused"),
], unit="ops", w=12, desc="A new unsupported operation after a workerd upgrade needs the shim to catch up.")

dashboard = {
    "title": "Selvedge",
    "uid": "selvedge",
    "description": "Selvedge components: traffic, runners, bindings and containers. Generated by tools/grafana-dashboard.py.",
    "tags": ["selvedge"],
    "editable": True,
    "schemaVersion": 39,
    "time": {"from": "now-6h", "to": "now"},
    "refresh": "30s",
    "templating": {"list": [
        {"name": "datasource", "label": "Data source", "type": "datasource", "query": "prometheus"},
        {"name": "namespace", "label": "Namespace", "type": "query", "datasource": DS,
         "query": {"query": "label_values(selvedge_http_requests_total, namespace)", "refId": "ns"},
         "definition": "label_values(selvedge_http_requests_total, namespace)",
         "refresh": 2, "includeAll": True, "multi": True, "allValue": ".*",
         "current": {"text": "All", "value": "$__all"}},
    ]},
    "panels": d.panels,
}

if sys.argv[1:] == ["--rules"]:
    rules = [{"record": f"check:panel{p['id']}_{i}",
              "expr": t["expr"].replace("$__rate_interval", "5m").replace("$namespace", ".*")}
             for p in d.panels for i, t in enumerate(p.get("targets", []))]
    print(json.dumps({"groups": [{"name": "selvedge-dashboard", "rules": rules}]}, indent=2))
else:
    print(json.dumps(dashboard, indent=2))
