BEGIN;
	ALTER TABLE notification ADD COLUMN id BIGSERIAL PRIMARY KEY;
COMMIT;
