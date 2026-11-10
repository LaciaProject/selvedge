{{- define "selvedge.name" -}}
{{- .Release.Name | trunc 40 | trimSuffix "-" -}}
{{- end -}}

{{- define "selvedge.labels" -}}
app.kubernetes.io/name: selvedge
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
{{- end -}}

{{- define "selvedge.selector" -}}
app.kubernetes.io/name: selvedge
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{- define "selvedge.ha" -}}
{{- if eq .Values.profile "ha" }}true{{ end -}}
{{- end -}}

{{/* Replicas for a component: the explicit value, else the profile default. */}}
{{- define "selvedge.replicas" -}}
{{- $explicit := index .root.Values.replicas .component -}}
{{- if not (kindIs "invalid" $explicit) -}}
{{- $explicit -}}
{{- else if eq .root.Values.profile "ha" -}}
{{- index (dict "api" 2 "data" 3 "gateway" 3 "runner" 3 "containers" 2) .component -}}
{{- else -}}
1
{{- end -}}
{{- end -}}

{{- define "selvedge.image" -}}
{{ .Values.image.repository }}:{{ .Values.image.tag | default .Chart.AppVersion }}
{{- end -}}

{{- define "selvedge.secretName" -}}
{{- .Values.secrets.existingSecret | default (printf "%s-secrets" (include "selvedge.name" .)) -}}
{{- end -}}

{{- define "selvedge.objectStoreSecret" -}}
{{- .Values.objectStore.existingSecret | default (printf "%s-object-store" (include "selvedge.name" .)) -}}
{{- end -}}

{{- define "selvedge.s3Endpoint" -}}
{{- if .Values.objectStore.bundled -}}
http://{{ include "selvedge.name" . }}-seaweedfs:8333
{{- else -}}
{{- required "objectStore.endpoint is required when objectStore.bundled is false" .Values.objectStore.endpoint -}}
{{- end -}}
{{- end -}}

{{/* Topology spread over zones and hosts for a component. */}}
{{- define "selvedge.spread" -}}
{{- $on := .root.Values.topologySpread.enabled -}}
{{- if kindIs "invalid" $on }}{{ $on = eq .root.Values.profile "ha" }}{{ end -}}
{{- if $on }}
topologySpreadConstraints:
  - maxSkew: 1
    topologyKey: {{ .root.Values.topologySpread.topologyKey }}
    whenUnsatisfiable: ScheduleAnyway
    labelSelector:
      matchLabels:
        {{- include "selvedge.selector" . | nindent 8 }}
  - maxSkew: 1
    topologyKey: kubernetes.io/hostname
    whenUnsatisfiable: ScheduleAnyway
    labelSelector:
      matchLabels:
        {{- include "selvedge.selector" . | nindent 8 }}
{{- end }}
{{- end -}}

{{/* Environment shared by every role. */}}
{{- define "selvedge.commonEnv" -}}
- name: SELVEDGE_INTERNAL_TOKEN
  valueFrom: { secretKeyRef: { name: {{ include "selvedge.secretName" . }}, key: internal-token } }
- name: RUST_LOG
  value: {{ .Values.logLevel | default "info" | quote }}
{{- if .Values.metrics.enabled }}
- name: SELVEDGE_METRICS_LISTEN
  value: 0.0.0.0:{{ .Values.metrics.port }}
{{- end }}
{{- end -}}

{{/* NetworkPolicy ingress to the metrics port, from metrics.scrapeFrom (default: anywhere). */}}
{{- define "selvedge.metricsIngress" -}}
{{- if .Values.metrics.enabled }}
- ports: [{ port: {{ .Values.metrics.port }} }]
  {{- with .Values.metrics.scrapeFrom }}
  from: {{- toYaml . | nindent 4 }}
  {{- end }}
{{- end }}
{{- end -}}

{{/* Pod annotations for Prometheus' Kubernetes service discovery. */}}
{{- define "selvedge.metricsAnnotations" -}}
{{- if and .Values.metrics.enabled .Values.metrics.annotations }}
annotations:
  prometheus.io/scrape: "true"
  prometheus.io/port: {{ .Values.metrics.port | quote }}
  prometheus.io/path: /metrics
{{- end }}
{{- end -}}

{{- define "selvedge.databaseEnv" -}}
- name: SELVEDGE_DATABASE_URL
  valueFrom:
    secretKeyRef:
      {{- if .Values.postgres.existingSecret }}
      name: {{ .Values.postgres.existingSecret }}
      {{- else }}
      name: {{ include "selvedge.name" . }}-database
      {{- end }}
      key: url
{{- end -}}

{{- define "selvedge.s3Env" -}}
- name: AWS_ENDPOINT
  value: {{ include "selvedge.s3Endpoint" . | quote }}
- name: AWS_REGION
  value: {{ .Values.objectStore.region | quote }}
- name: AWS_ACCESS_KEY_ID
  valueFrom: { secretKeyRef: { name: {{ include "selvedge.objectStoreSecret" . }}, key: access-key-id } }
- name: AWS_SECRET_ACCESS_KEY
  valueFrom: { secretKeyRef: { name: {{ include "selvedge.objectStoreSecret" . }}, key: secret-access-key } }
{{- end -}}

{{- define "selvedge.pod" -}}
securityContext:
  {{- toYaml .root.Values.podSecurityContext | nindent 2 }}
enableServiceLinks: false
{{- include "selvedge.spread" . }}
{{- end -}}

{{- define "selvedge.peer" -}}
- podSelector:
    matchLabels:
      app.kubernetes.io/name: selvedge
      app.kubernetes.io/instance: {{ .root.Release.Name }}
      app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{- /* mTLS between components (tls.enabled). */}}
{{- define "selvedge.tlsEnv" -}}
{{- if .Values.tls.enabled }}
- name: SELVEDGE_TLS_CERT
  value: /etc/selvedge/tls/tls.crt
- name: SELVEDGE_TLS_KEY
  value: /etc/selvedge/tls/tls.key
- name: SELVEDGE_TLS_CA
  value: /etc/selvedge/tls/ca.crt
{{- end }}
{{- end -}}

{{- define "selvedge.tlsMount" -}}
{{- if .Values.tls.enabled }}
- { name: tls, mountPath: /etc/selvedge/tls, readOnly: true }
{{- end }}
{{- end -}}

{{- /* The certificate of one role: (dict "root" . "role" "api"). */}}
{{- define "selvedge.tlsVolume" -}}
{{- if .root.Values.tls.enabled }}
- name: tls
  secret: { secretName: {{ include "selvedge.name" .root }}-tls-{{ .role }}, defaultMode: 0444 }
{{- end }}
{{- end -}}

{{- /* The API's internal endpoints, as agents and gateways reach them. */}}
{{- define "selvedge.apiInternalUrl" -}}
{{- if .Values.tls.enabled -}}
https://{{ include "selvedge.name" . }}-api-internal:8786
{{- else -}}
http://{{ include "selvedge.name" . }}-api:{{ .Values.api.service.port }}
{{- end -}}
{{- end -}}

{{- /* The gateway, as the API and data service reach it for internal routing. */}}
{{- define "selvedge.gatewayInternalUrl" -}}
{{- if .Values.tls.enabled -}}
https://{{ include "selvedge.name" . }}-gateway-internal:8779
{{- else -}}
http://{{ include "selvedge.name" . }}-gateway:{{ .Values.gateway.service.port }}
{{- end -}}
{{- end -}}

{{- /* Probes for a listener that requires mTLS: the kubelet cannot present
a client certificate, so curl does, with the component's own; a listener lets
its own role reach /healthz only. (dict "root" . "port" N "role" "data"). */}}
{{- define "selvedge.probes" -}}
{{- $p := .port -}}
{{- $host := printf "%s.selvedge.internal" .role -}}
{{- if .root.Values.tls.enabled }}
readinessProbe:
  exec: { command: [curl, -sf, --max-time, "2", --cacert, /etc/selvedge/tls/ca.crt, --cert, /etc/selvedge/tls/tls.crt, --key, /etc/selvedge/tls/tls.key, --resolve, "{{ $host }}:{{ $p }}:127.0.0.1", "https://{{ $host }}:{{ $p }}/healthz"] }
  periodSeconds: 2
  failureThreshold: {{ .failureThreshold | default 3 }}
livenessProbe:
  exec: { command: [curl, -sf, --max-time, "2", --cacert, /etc/selvedge/tls/ca.crt, --cert, /etc/selvedge/tls/tls.crt, --key, /etc/selvedge/tls/tls.key, --resolve, "{{ $host }}:{{ $p }}:127.0.0.1", "https://{{ $host }}:{{ $p }}/healthz"] }
  periodSeconds: 10
  failureThreshold: 6
{{- else }}
readinessProbe: { httpGet: { path: /healthz, port: http }, periodSeconds: 2, failureThreshold: {{ .failureThreshold | default 3 }} }
livenessProbe: { httpGet: { path: /healthz, port: http }, periodSeconds: 10, failureThreshold: 6 }
{{- end }}
{{- end -}}

{{- /* The containers controller, as runners reach it. */}}
{{- define "selvedge.containersUrl" -}}
{{- if .Values.tls.enabled -}}
https://{{ include "selvedge.name" . }}-containers:8792
{{- else -}}
http://{{ include "selvedge.name" . }}-containers:8792
{{- end -}}
{{- end -}}

{{- /* The Secret with the key and certificate of registry tokens. */}}
{{- define "selvedge.registryTokenSecret" -}}
{{- .Values.containers.registry.tokenSecret | default (printf "%s-registry-token" (include "selvedge.name" .)) -}}
{{- end -}}

{{- define "selvedge.registryHost" -}}
{{- required "containers.registry.host is required with containers.enabled: the registry's host as clients and nodes reach it" .Values.containers.registry.host -}}
{{- end -}}

{{- define "selvedge.registryTokenRealm" -}}
{{- required "containers.registry.tokenRealm is required with containers.enabled: the API's public URL followed by /registry/token" .Values.containers.registry.tokenRealm -}}
{{- end -}}
