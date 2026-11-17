-- Explicit operator resolution knows cost, not missing usage. No history is resolved here.
ALTER TABLE governance_reservations ADD COLUMN free_price_resolved boolean NOT NULL DEFAULT false;
-- The original unnamed multi-column check also requires complete token usage.
-- Find only that check on the parent; ALTER propagates to every partition.
DO $$ DECLARE constraint_name text; BEGIN
 SELECT conname INTO STRICT constraint_name FROM pg_constraint
 WHERE conrelid='public.governance_reservations'::regclass AND contype='c'
 AND pg_get_constraintdef(oid) LIKE '%state%settled%input_tokens IS NOT NULL%output_tokens IS NOT NULL%';
 EXECUTE format('ALTER TABLE public.governance_reservations DROP CONSTRAINT %I',constraint_name);
END $$;
ALTER TABLE governance_reservations
 ADD CONSTRAINT reservation_settled_usage CHECK (
  (state='settled' AND actual_microusd IS NOT NULL
   AND (free_price_resolved OR (input_tokens IS NOT NULL AND output_tokens IS NOT NULL)))
  OR (state<>'settled' AND actual_microusd IS NULL)),
 ADD CONSTRAINT reservation_free_price_resolution CHECK (
  NOT free_price_resolved OR (state='settled' AND actual_microusd=0
   AND held_microusd IS NOT DISTINCT FROM 0 AND price_id IS NOT NULL AND NOT unbounded_cost));
