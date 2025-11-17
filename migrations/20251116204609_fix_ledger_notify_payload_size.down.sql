-- Restore original notify function that sends full row
-- Note: This will reintroduce the payload size issue for large transactions

CREATE OR REPLACE FUNCTION notify_sqlx_ledger_events() RETURNS TRIGGER AS $$
DECLARE
  payload TEXT;
BEGIN
  payload := row_to_json(NEW);
  PERFORM pg_notify('sqlx_ledger_events', payload);
  RETURN NULL;
END;
$$ LANGUAGE plpgsql;
