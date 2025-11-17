-- Fix pg_notify payload size issue by sending only essential data
-- instead of the full row, which can exceed 8KB limit with large transactions

CREATE OR REPLACE FUNCTION notify_sqlx_ledger_events() RETURNS TRIGGER AS $$
DECLARE
  payload TEXT;
BEGIN
  payload := json_build_object(
    'id', NEW.id,
    'type', NEW.type,
    'recorded_at', NEW.recorded_at
  )::text;
  PERFORM pg_notify('sqlx_ledger_events', payload);
  RETURN NULL;
END;
$$ LANGUAGE plpgsql;
