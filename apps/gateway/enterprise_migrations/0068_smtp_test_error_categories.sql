-- #411: record refused SMTP tests instead of rolling back their outcome.
-- Match alert_deliveries.error after 0049; preserve existing rows and NULL.
ALTER TABLE installation_settings
 DROP CONSTRAINT installation_settings_smtp_last_test_error_check;
ALTER TABLE installation_settings
 ADD CONSTRAINT installation_settings_smtp_last_test_error_check
 CHECK(smtp_last_test_error IS NULL OR smtp_last_test_error IN
 ('credential','address','connection','tls','authentication','rejected','timeout','interrupted','host_not_allowed'));
