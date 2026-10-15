ALTER TABLE bbs_groups ADD COLUMN archived_at TEXT;

-- Keep the existing all-time name uniqueness constraint; archived names stay reserved.
-- Active listings and name lookups can skip archived groups.
CREATE INDEX bbs_groups_active_name ON bbs_groups (name) WHERE archived_at IS NULL;
