BEGIN;
	ALTER TABLE post ADD COLUMN had_href BOOLEAN;
COMMIT;