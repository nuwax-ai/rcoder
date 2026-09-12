-- Writers must be drained before migration; older writers do not preserve generations.
ALTER TABLE userapp_metadata ADD COLUMN generation text;
UPDATE userapp_metadata SET generation = gen_random_uuid()::text;
ALTER TABLE userapp_metadata ALTER COLUMN generation SET NOT NULL;
