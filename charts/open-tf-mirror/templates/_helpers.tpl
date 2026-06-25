{{/*
Parse image tag for app.kubernetes.io/version labels.
*/}}
{{- define "open-tf-mirror.parseImageTag" -}}
{{- regexReplaceAll "[^a-zA-Z0-9-_.]+" (regexReplaceAll "@sha256:[a-f0-9]+" .image.tag "") "" -}}
{{- end -}}

{{/*
Build image reference, honoring global.imageRegistry.
*/}}
{{- define "open-tf-mirror.image" -}}
{{- if .Values.global.imageRegistry -}}
{{- printf "%s/%s:%s" .Values.global.imageRegistry .image.repository (default "latest" .image.tag) -}}
{{- else -}}
{{- printf "%s:%s" .image.repository (default "latest" .image.tag) -}}
{{- end -}}
{{- end -}}

{{/*
Resolve PVC storageClass, honoring global.storageClass.
*/}}
{{- define "open-tf-mirror.storageClass" -}}
{{- default .Values.global.storageClass .pvc.storageClass -}}
{{- end -}}

{{/*
Expand the chart name.
*/}}
{{- define "open-tf-mirror.commonName" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Create a fully qualified base name.
*/}}
{{- define "open-tf-mirror.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := include "open-tf-mirror.commonName" . -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Resolve namespace.
*/}}
{{- define "open-tf-mirror.namespace" -}}
{{- default .Release.Namespace .Values.namespaceOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Create chart label.
*/}}
{{- define "open-tf-mirror.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
open-tf-mirror resource name.
*/}}
{{- define "open-tf-mirror.name" -}}
{{- include "open-tf-mirror.fullname" . -}}
{{- end -}}

{{/*
Common labels.
*/}}
{{- define "open-tf-mirror.labels" -}}
helm.sh/chart: {{ include "open-tf-mirror.chart" . }}
app.kubernetes.io/part-of: open-tf-mirror
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{/*
Selector labels.
*/}}
{{- define "open-tf-mirror.selectorLabels" -}}
{{ include "open-tf-mirror.labels" . }}
app.kubernetes.io/component: server
{{- if .Values.commonLabels }}
{{ toYaml .Values.commonLabels }}
{{- end }}
{{- end -}}
