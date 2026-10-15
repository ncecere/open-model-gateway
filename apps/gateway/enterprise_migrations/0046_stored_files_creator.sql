-- #76, #74: find a person's uploads by creator (cleanup clears their names;
-- the subject export lists them) without scanning every stored file.
CREATE INDEX stored_files_by_creator ON stored_files(created_by_user_id, created_at);
