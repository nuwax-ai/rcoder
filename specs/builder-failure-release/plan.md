# Implementation

Keep structured rejection evidence in shared_types and pass it through runtime errors.
Classify kube/bollard status responses before string conversion; never parse messages.
Use one builder completion helper for K8s ConfigMap leases and Docker file markers.
Unclassified errors fail closed. Preserve SDK error values on Docker create/start.
Propagate failed PVC force-delete rather than proceeding after an uncertain write.
On cached Running creation, reconcile Service explicitly and use the read-only info
helper; read-side best-effort self-heal must not hide an uncertain mutation.
Do not alter Drop to schedule remote deletion. Leave production operation guards unchanged.

Regression: explicit 403 releases UID/version-bound lease; lost PATCH response and 500
retain it; Docker marker allows retry only after explicit rejection; release failure
preserves original error and marker ownership. Bound entire HTTP test exchanges.
