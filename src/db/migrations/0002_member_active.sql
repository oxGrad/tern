-- A member is active while listed in members.toml or on the team. Only
-- active members are scored or count toward the team median.
ALTER TABLE members ADD COLUMN in_toml BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE members ADD COLUMN in_team BOOLEAN NOT NULL DEFAULT FALSE;
UPDATE members SET in_toml = TRUE;
