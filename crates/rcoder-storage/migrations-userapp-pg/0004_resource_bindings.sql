-- No cascading deletion: retained physical ownership fences prevent a replacement
-- lifecycle from claiming an older container/workload UID.
CREATE TABLE userapp_resource_bindings (
    service_type TEXT NOT NULL,
    physical_uid TEXT NOT NULL,
    app_id TEXT NOT NULL REFERENCES userapp_lifecycles(app_id),
    record TEXT NOT NULL,
    PRIMARY KEY (service_type, physical_uid)
);
CREATE INDEX userapp_resource_bindings_app ON userapp_resource_bindings(app_id);
