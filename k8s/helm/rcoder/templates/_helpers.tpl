{{/*
Helper function for image
*/}}
{{- define "rcoder.image" -}}
{{- printf "%s/%s:%s" .Values.image.registry .Values.image.repository .Values.image.tag -}}
{{- end }}

{{/*
Helper function for agent runner image
*/}}
{{- define "rcoder.agentRunner.image" -}}
{{- printf "%s/%s:%s" .Values.image.agentRunner.registry .Values.image.agentRunner.repository .Values.image.agentRunner.tag -}}
{{- end }}

{{/*
Helper function for PostgreSQL METAURL
*/}}
{{- define "rcoder.postgresql.metaurl" -}}
{{- if .Values.postgresql.external }}
{{- printf "postgres://%s:%s@%s:%d/%s" .Values.postgresql.auth.username .Values.postgresql.auth.password .Values.postgresql.host .Values.postgresql.port .Values.postgresql.auth.database -}}
{{- else }}
{{- printf "postgres://%s:%s@postgresql.%s.svc.cluster.local:5432/%s" .Values.postgresql.auth.username .Values.postgresql.auth.password .Values.namespace .Values.postgresql.auth.database -}}
{{- end }}
{{- end }}

{{/*
Helper function for MinIO endpoint
*/}}
{{- define "rcoder.minio.endpoint" -}}
{{- if .Values.minio.external }}
{{- .Values.minio.endpoint -}}
{{- else }}
{{- printf "http://minio.%s.svc.cluster.local:9000" .Values.namespace -}}
{{- end }}
{{- end }}
