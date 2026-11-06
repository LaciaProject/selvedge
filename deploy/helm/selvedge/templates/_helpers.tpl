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
{{- index (dict "api" 2 "data" 3 "gateway" 3 "runner" 3) .component -}}
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
  value: info
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
