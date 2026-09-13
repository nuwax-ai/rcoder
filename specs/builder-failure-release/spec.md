# Builder failure completion contract

A confirmed runtime API rejection ends the builder operation and permits retry.
An unknown/transport outcome, panic or cancellation retains the durable recovery fence.
HTTP 408 and 499 are not evidence of rejection. HTTP 5xx remains unconfirmed.
A release failure must not replace an existing creation error, nor report success.
Docker file unlock alone is not completion: only confirmed completion clears its marker.
Existing workload, data and production-family lifecycle policy remain outside this change.
