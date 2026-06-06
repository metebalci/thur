{{/* Chart name, overridable by fullnameOverride is intentionally omitted — the
release name is fixed for a singleton storage driver. */}}
{{- define "thurvsa-csi.name" -}}
thurvsa-csi
{{- end -}}

{{- define "thurvsa-csi.fullname" -}}
{{- printf "%s-%s" .Release.Name (include "thurvsa-csi.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "thurvsa-csi.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
app.kubernetes.io/name: {{ include "thurvsa-csi.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
{{- end -}}

{{/* Resolved driver image (tag defaults to the chart appVersion). */}}
{{- define "thurvsa-csi.image" -}}
{{- $tag := .Values.driver.image.tag | default .Chart.AppVersion -}}
{{- printf "%s:%s" .Values.driver.image.repository $tag -}}
{{- end -}}

{{/* The required host thurvsa gid, validated at render time. */}}
{{- define "thurvsa-csi.gid" -}}
{{- required "thurvsaGid is required: set it to the host `thurvsa` group's numeric gid (getent group thurvsa) so the controller can reach the peer-cred admin socket" .Values.thurvsaGid -}}
{{- end -}}

{{- define "thurvsa-csi.controllerServiceAccount" -}}
{{ include "thurvsa-csi.fullname" . }}-controller
{{- end -}}

{{- define "thurvsa-csi.nodeServiceAccount" -}}
{{ include "thurvsa-csi.fullname" . }}-node
{{- end -}}
