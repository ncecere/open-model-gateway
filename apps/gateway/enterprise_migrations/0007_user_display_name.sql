-- Display name from the identity provider's verified ID token (`name` claim,
-- standard `profile` scope), refreshed at every sign-in and cleared with the
-- email when an account is cleaned up. Presentation only: never used for
-- authorization, matching or entitlement. NULL when the provider sends none.
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
ALTER TABLE users ADD COLUMN display_name text
 CHECK(display_name IS NULL OR (char_length(display_name) BETWEEN 1 AND 200
  AND display_name !~ '[[:cntrl:]]' AND display_name = btrim(display_name)));
