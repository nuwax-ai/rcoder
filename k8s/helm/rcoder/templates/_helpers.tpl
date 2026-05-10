{{/*
RCoder chart 公共 helper
*/}}

{{/* 应用全名 (Release + chart) */}}
{{- define "rcoder.fullname" -}}
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

{{/* Chart 标识 */}}
{{- define "rcoder.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* 通用标签 (所有资源都带) */}}
{{- define "rcoder.labels" -}}
helm.sh/chart: {{ include "rcoder.chart" . }}
app.kubernetes.io/name: rcoder
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
environment: {{ .Values.environment | quote }}
{{- end -}}

{{/* 应用层的 selector label (app=rcoder 与 Kustomize 完全一致, 保证 Service 选得到 pod) */}}
{{- define "rcoder.selectorLabels" -}}
app: rcoder
component: rcoder-main
{{- end -}}

{{/* ============================================================
     镜像地址拼装
     rcoder-own image  -> {{ .Values.global.imageRegistry }}/{repo}:{tag}
     third-party image -> {{ .Values.global.thirdPartyRegistry }}/{repo}:{tag}
                          (若 thirdPartyRegistry 为空, 保留原始 repo)
     ============================================================ */}}

{{/* 自有镜像: 入参 dict {repository, tag, chartContext} */}}
{{- define "rcoder.ownImage" -}}
{{- $repo := .repository -}}
{{- $tag := .tag -}}
{{- $registry := .ctx.Values.global.imageRegistry -}}
{{- if empty $tag -}}{{- $tag = .ctx.Chart.AppVersion -}}{{- end -}}
{{- if $registry -}}
{{- printf "%s/%s:%s" $registry $repo $tag -}}
{{- else -}}
{{- printf "%s:%s" $repo $tag -}}
{{- end -}}
{{- end -}}

{{/* 第三方镜像: 入参 dict {repository, tag, ctx} */}}
{{- define "rcoder.thirdPartyImage" -}}
{{- $repo := .repository -}}
{{- $tag := .tag -}}
{{- $registry := .ctx.Values.global.thirdPartyRegistry -}}
{{- if $registry -}}
{{- printf "%s/%s:%s" $registry $repo $tag -}}
{{- else -}}
{{- printf "%s:%s" $repo $tag -}}
{{- end -}}
{{- end -}}

{{/* 快捷: rcoder 主服务 image */}}
{{- define "rcoder.image" -}}
{{- include "rcoder.ownImage" (dict "repository" .Values.rcoder.image.repository "tag" .Values.rcoder.image.tag "ctx" .) -}}
{{- end -}}

{{/* 快捷: agent-runner image (供 rcoder env var 使用) */}}
{{- define "rcoder.agentRunnerImage" -}}
{{- include "rcoder.ownImage" (dict "repository" .Values.agentRunner.image.repository "tag" .Values.agentRunner.image.tag "ctx" .) -}}
{{- end -}}

{{/* 镜像仓库前缀 (config.yml 里 docker_config.multi_image_config.global_defaults.registry_prefix 使用) */}}
{{- define "rcoder.registryPrefix" -}}
{{- default "" .Values.global.imageRegistry -}}
{{- end -}}

{{/* ============================================================
     StorageClass 名 (集群级资源, Release 之间必须独立)
     - 用户显式指定 juicefs.storageClass.name -> 照用
     - 否则回退到 "juicefs-sc-{release}"
     ============================================================ */}}
{{- define "rcoder.storageClassName" -}}
{{- if .Values.juicefs.storageClass.name -}}
{{- .Values.juicefs.storageClass.name -}}
{{- else -}}
{{- printf "juicefs-sc-%s" .Release.Name -}}
{{- end -}}
{{- end -}}

{{/* ============================================================
     ClusterRoleBinding 名 (集群级资源, 同上)
     ============================================================ */}}
{{- define "rcoder.clusterRoleBindingName" -}}
{{- printf "%s-pods-crb" .Release.Name -}}
{{- end -}}

{{/* ClusterRole 共享名 —— 多个 release 绑到同一个 ClusterRole, 规则相同故无冲突 */}}
{{- define "rcoder.clusterRoleName" -}}
rcoder-pods-clusterrole
{{- end -}}

{{/* imagePullSecrets 渲染块 */}}
{{- define "rcoder.imagePullSecrets" -}}
{{- if .Values.global.imagePullSecrets }}
imagePullSecrets:
{{- range .Values.global.imagePullSecrets }}
  - name: {{ .name }}
{{- end }}
{{- end -}}
{{- end -}}

{{/* PostgreSQL service 主机名 (namespace 内) */}}
{{- define "rcoder.postgresqlHost" -}}
{{- printf "postgresql.%s.svc.cluster.local" .Release.Namespace -}}
{{- end -}}

{{/* MinIO service 主机名 */}}
{{- define "rcoder.minioHost" -}}
{{- printf "minio-service.%s.svc.cluster.local" .Release.Namespace -}}
{{- end -}}

{{/* JuiceFS metaurl */}}
{{- define "rcoder.juicefsMetaurl" -}}
{{- printf "postgres://%s:%s@postgresql.%s:5432/juicefs" .Values.credentials.postgresql.user .Values.credentials.postgresql.password .Release.Namespace -}}
{{- end -}}

{{/* JuiceFS bucket URL */}}
{{- define "rcoder.juicefsBucketUrl" -}}
{{- printf "http://minio-service.%s:9000/%s" .Release.Namespace .Values.juicefs.bucket -}}
{{- end -}}
