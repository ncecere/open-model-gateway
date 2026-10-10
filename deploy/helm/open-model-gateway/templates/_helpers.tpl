{{/*
Chart name.
*/}}
{{- define "omg.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "omg.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "omg.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "omg.labels" -}}
app.kubernetes.io/name: {{ include "omg.name" . }}
helm.sh/chart: {{ include "omg.chart" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "omg.selectorLabels" -}}
app.kubernetes.io/name: {{ include "omg.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
The primary gateway workload's own selector (Deployment/Service/PDB/
NetworkPolicy/ServiceMonitor/PodMonitor). Every other component (migrate Job,
CNPG Pooler, loadtest mock-upstream/loadgen) adds its own
`app.kubernetes.io/component` label on top of the bare omg.selectorLabels;
without the same qualifier here, those selectors (matchLabels only checks a
subset) would also match this chart's other pods sharing name+instance, for
example routing mock-upstream traffic through the gateway Service or
over/under-counting the gateway PodDisruptionBudget during a migration Job run.
*/}}
{{- define "omg.gatewaySelectorLabels" -}}
{{ include "omg.selectorLabels" . }}
app.kubernetes.io/component: gateway
{{- end -}}

{{- define "omg.serviceAccountName" -}}
{{- if .Values.gateway.serviceAccount.create -}}
{{- default (include "omg.fullname" .) .Values.gateway.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.gateway.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/*
Image reference: digest wins over tag; both fall back to appVersion.
*/}}
{{- define "omg.image" -}}
{{- if .Values.image.digest -}}
{{ .Values.image.repository }}@{{ .Values.image.digest }}
{{- else -}}
{{ .Values.image.repository }}:{{ .Values.image.tag | default .Chart.AppVersion }}
{{- end -}}
{{- end -}}

{{/*
CNPG-managed resource names.
*/}}
{{- define "omg.cnpg.clusterName" -}}
{{ include "omg.fullname" . }}-db
{{- end -}}

{{- define "omg.cnpg.poolerName" -}}
{{ include "omg.fullname" . }}-pooler
{{- end -}}

{{- define "omg.configMapName" -}}
{{ include "omg.fullname" . }}-env
{{- end -}}

{{- define "omg.grantsConfigMapName" -}}
{{ include "omg.fullname" . }}-runtime-grants
{{- end -}}

{{/*
Database connection wiring.

External mode (cnpg.cluster.enabled=false): every URL comes from a single
key in an operator-provided existingSecret (secrets.database.*,
secrets.migrator), injected with secretKeyRef or DATABASE_URL_FILE.

CNPG mode (cnpg.cluster.enabled=true): the chart never generates a secret.
Operators pre-create one basic-auth Secret per role (cnpg.cluster.roles.*),
referenced by name, which CNPG's `bootstrap.initdb.secret` (migrator/owner)
and `managed.roles[].passwordSecret` (runtime) consume directly. Full
connection strings are composed in-pod with Kubernetes' `$(VAR)` dependent
environment variable substitution (one secretKeyRef per username/password,
then a literal `value` referencing them) so passwords are never written into
chart-rendered YAML or an init container script. See docs/kubernetes.md
"CloudNativePG wiring".
*/}}

{{/* Env entries exposing a role's username/password from its secret. */}}
{{- define "omg.cnpg.roleCredEnv" -}}
{{- $role := .role -}}
{{- $prefix := .prefix -}}
- name: {{ $prefix }}_USERNAME
  valueFrom:
    secretKeyRef:
      name: {{ $role.existingSecret }}
      key: {{ $role.usernameKey | default "username" }}
- name: {{ $prefix }}_PASSWORD
  valueFrom:
    secretKeyRef:
      name: {{ $role.existingSecret }}
      key: {{ $role.passwordKey | default "password" }}
{{- end -}}

{{/* postgres:// URL built from $(PREFIX_USERNAME)/$(PREFIX_PASSWORD) + a static host/db. */}}
{{- define "omg.cnpg.url" -}}
{{- $prefix := .prefix -}}
{{- $host := .host -}}
{{- $db := .db -}}
{{- $sslmode := .sslmode -}}
postgres://$({{ $prefix }}_USERNAME):$({{ $prefix }}_PASSWORD)@{{ $host }}:5432/{{ $db }}{{ if $sslmode }}?sslmode={{ $sslmode }}{{ end }}
{{- end -}}

{{/*
Env entries for the gateway Deployment's runtime DATABASE_URL
(pooled traffic through the CNPG Pooler, or an external existingSecret key).
*/}}
{{- define "omg.env.runtimeDatabase" -}}
{{- if .Values.cnpg.cluster.enabled -}}
{{ include "omg.cnpg.roleCredEnv" (dict "role" .Values.cnpg.cluster.roles.runtime "prefix" "OMG_DB_RUNTIME") }}
- name: DATABASE_URL
  value: {{ include "omg.cnpg.url" (dict "prefix" "OMG_DB_RUNTIME" "host" (include "omg.cnpg.poolerName" .) "db" .Values.cnpg.cluster.database "sslmode" .Values.cnpg.cluster.sslMode) | quote }}
{{- else -}}
- name: DATABASE_URL
  valueFrom:
    secretKeyRef:
      name: {{ required "secrets.database.runtime.existingSecret is required when cnpg.cluster.enabled is false" .Values.secrets.database.runtime.existingSecret }}
      key: {{ .Values.secrets.database.runtime.urlKey }}
{{- end -}}
{{- end -}}

{{/*
Env entries for GATEWAY_LISTEN_DATABASE_URL: always a direct, unpooled
session connection (never through the transaction-mode Pooler).
*/}}
{{- define "omg.env.listenDatabase" -}}
{{- if .Values.cnpg.cluster.enabled -}}
{{ include "omg.cnpg.roleCredEnv" (dict "role" .Values.cnpg.cluster.roles.runtime "prefix" "OMG_DB_LISTEN") }}
- name: GATEWAY_LISTEN_DATABASE_URL
  value: {{ include "omg.cnpg.url" (dict "prefix" "OMG_DB_LISTEN" "host" (printf "%s-rw" (include "omg.cnpg.clusterName" .)) "db" .Values.cnpg.cluster.database "sslmode" .Values.cnpg.cluster.sslMode) | quote }}
{{- else -}}
- name: GATEWAY_LISTEN_DATABASE_URL
  valueFrom:
    secretKeyRef:
      name: {{ required "secrets.database.listen.existingSecret is required when cnpg.cluster.enabled is false" .Values.secrets.database.listen.existingSecret }}
      key: {{ .Values.secrets.database.listen.urlKey }}
{{- end -}}
{{- end -}}

{{/*
Env entries for the optional GATEWAY_REPORTING_DATABASE_URL (CNPG -ro
service, or an external existingSecret key).
*/}}
{{- define "omg.env.reportingDatabase" -}}
{{- if .Values.secrets.database.reporting.enabled -}}
{{- if .Values.cnpg.cluster.enabled -}}
{{ include "omg.cnpg.roleCredEnv" (dict "role" .Values.cnpg.cluster.roles.runtime "prefix" "OMG_DB_REPORTING") }}
- name: GATEWAY_REPORTING_DATABASE_URL
  value: {{ include "omg.cnpg.url" (dict "prefix" "OMG_DB_REPORTING" "host" (printf "%s-ro" (include "omg.cnpg.clusterName" .)) "db" .Values.cnpg.cluster.database "sslmode" .Values.cnpg.cluster.sslMode) | quote }}
{{- else -}}
- name: GATEWAY_REPORTING_DATABASE_URL
  valueFrom:
    secretKeyRef:
      name: {{ required "secrets.database.reporting.existingSecret is required when secrets.database.reporting.enabled is true and cnpg.cluster.enabled is false" .Values.secrets.database.reporting.existingSecret }}
      key: {{ .Values.secrets.database.reporting.urlKey }}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Env entries for the migration Job's DATABASE_URL: always a direct
connection as the migrator role (CNPG -rw service, or an external
existingSecret key). Never the runtime role, never the Pooler.
*/}}
{{- define "omg.env.migratorDatabase" -}}
{{- if .Values.cnpg.cluster.enabled -}}
{{ include "omg.cnpg.roleCredEnv" (dict "role" .Values.cnpg.cluster.roles.migrator "prefix" "OMG_DB_MIGRATOR") }}
- name: DATABASE_URL
  value: {{ include "omg.cnpg.url" (dict "prefix" "OMG_DB_MIGRATOR" "host" (printf "%s-rw" (include "omg.cnpg.clusterName" .)) "db" .Values.cnpg.cluster.database "sslmode" .Values.cnpg.cluster.sslMode) | quote }}
{{- else -}}
- name: DATABASE_URL
  valueFrom:
    secretKeyRef:
      name: {{ required "secrets.migrator.existingSecret is required (a separate migrator credential; never the runtime role)" .Values.secrets.migrator.existingSecret }}
      key: {{ .Values.secrets.migrator.urlKey }}
{{- end -}}
{{- end -}}
