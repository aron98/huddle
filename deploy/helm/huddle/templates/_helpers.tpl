{{/* Standard labels + a name that respects fullnameOverride. */}}
{{- define "huddle.fullname" -}}
{{- default "huddle" .Values.fullnameOverride -}}
{{- end -}}
