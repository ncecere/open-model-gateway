-- #405: management text limits count characters, matching the API contract's
-- maxLength. The provider-cost text columns were the only management text
-- CHECKs that counted bytes (octet_length). Every existing value satisfies the
-- new constraint (a value of at most N bytes has at most N characters), so no
-- data changes; the constraints keep their names.
ALTER TABLE provider_cost_imports
 DROP CONSTRAINT provider_cost_imports_account_id_check,
 DROP CONSTRAINT provider_cost_imports_evidence_reference_check;
ALTER TABLE provider_cost_imports
 ADD CONSTRAINT provider_cost_imports_account_id_check
 CHECK(char_length(account_id) BETWEEN 1 AND 200),
 ADD CONSTRAINT provider_cost_imports_evidence_reference_check
 CHECK(char_length(evidence_reference) BETWEEN 1 AND 256);

ALTER TABLE provider_cost_mappings
 DROP CONSTRAINT provider_cost_mappings_account_id_check,
 DROP CONSTRAINT provider_cost_mappings_evidence_reference_check;
ALTER TABLE provider_cost_mappings
 ADD CONSTRAINT provider_cost_mappings_account_id_check
 CHECK(char_length(account_id) BETWEEN 1 AND 200),
 ADD CONSTRAINT provider_cost_mappings_evidence_reference_check
 CHECK(char_length(evidence_reference) BETWEEN 1 AND 256);

ALTER TABLE provider_cost_bindings
 DROP CONSTRAINT provider_cost_bindings_provider_workspace_id_check,
 DROP CONSTRAINT provider_cost_bindings_evidence_reference_check;
ALTER TABLE provider_cost_bindings
 ADD CONSTRAINT provider_cost_bindings_provider_workspace_id_check
 CHECK(char_length(provider_workspace_id) BETWEEN 1 AND 4096),
 ADD CONSTRAINT provider_cost_bindings_evidence_reference_check
 CHECK(char_length(evidence_reference) BETWEEN 1 AND 256);
