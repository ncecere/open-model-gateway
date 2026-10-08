-- Deep-link return after sign-in. A login attempt may carry the same-origin
-- dashboard path the browser started from (validated by the server before it
-- is stored and again before redirecting); the callback returns there instead
-- of "/". Never an absolute URL, a protocol-relative URL or an API path.
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
ALTER TABLE oidc_login_attempts ADD COLUMN return_to text
 CHECK(return_to IS NULL OR (char_length(return_to) BETWEEN 1 AND 2048
  AND left(return_to,1)='/' AND left(return_to,2) NOT IN ('//','/\')
  AND return_to !~ '[[:cntrl:]\\]'));
