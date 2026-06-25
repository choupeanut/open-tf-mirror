{{/*
Parse image tag for app.kubernetes.io/version labels.
*/}}
{{- define "hermitcrab.parseImageTag" -}}
{{- regexReplaceAll "[^a-zA-Z0-9-_.]+" (regexReplaceAll "@sha256:[a-f0-9]+" .image.tag "") "" -}}
{{- end -}}

{{/*
Build image reference, honoring global.imageRegistry.
*/}}
{{- define "hermitcrab.image" -}}
{{- if .Values.global.imageRegistry -}}
{{- printf "%s/%s:%s" .Values.global.imageRegistry .image.repository (default "latest" .image.tag) -}}
{{- else -}}
{{- printf "%s:%s" .image.repository (default "latest" .image.tag) -}}
{{- end -}}
{{- end -}}

{{/*
Resolve PVC storageClass, honoring global.storageClass.
*/}}
{{- define "hermitcrab.storageClass" -}}
{{- default .Values.global.storageClass .pvc.storageClass -}}
{{- end -}}

{{/*
Expand the chart name.
*/}}
{{- define "hermitcrab.commonName" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Create a fully qualified base name.
*/}}
{{- define "hermitcrab.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := include "hermitcrab.commonName" . -}}
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
{{- define "hermitcrab.namespace" -}}
{{- default .Release.Namespace .Values.namespaceOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Create chart label.
*/}}
{{- define "hermitcrab.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Hermit Crab resource name. With fullnameOverride=hermitcrab and hermitcrab.name=hermitcrab this is hermitcrab-hermitcrab.
*/}}
{{- define "hermitcrab.name" -}}
{{- printf "%s-%s" (include "hermitcrab.fullname" .) .Values.hermitcrab.name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Common labels.
*/}}
{{- define "hermitcrab.labels" -}}
helm.sh/chart: {{ include "hermitcrab.chart" . }}
app.kubernetes.io/part-of: hermitcrab
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{/*
Selector labels.
*/}}
{{- define "hermitcrab.selectorLabels" -}}
{{ include "hermitcrab.labels" . }}
app.kubernetes.io/component: server
{{- if .Values.commonLabels }}
{{ toYaml .Values.commonLabels }}
{{- end }}
{{- end -}}
