-- Slot purchases (services::invest). A pending purchase holds its slots until expires_at, so
-- two buyers can't pay for the same slot; reserving locks the invest_assets row first.
--   pending   checkout open, slots held
--   paid      Korapay confirmed; the slots are the buyer's
--   expired   never paid in time; slots free again (a late payment is still honoured if the
--             slots are free, refunded if not)
--   failed    Korapay reports the charge failed, or the checkout could not be opened
--   refunded  paid, but the slots had gone to someone else; the money was sent back
CREATE TABLE back_invest_purchases (
    id                BIGSERIAL PRIMARY KEY,
    payment_reference TEXT NOT NULL UNIQUE,
    owner_id          UUID NOT NULL,
    asset_id          UUID NOT NULL,
    slot_ids          BIGINT[] NOT NULL CHECK (cardinality(slot_ids) > 0),
    slot_price        BIGINT NOT NULL CHECK (slot_price > 0),
    amount            BIGINT NOT NULL CHECK (amount > 0),
    status            TEXT NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'paid', 'expired', 'failed', 'refunded')),
    failure           TEXT,
    expires_at        TIMESTAMPTZ NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    paid_at           TIMESTAMPTZ
);

CREATE INDEX back_invest_purchases_open ON back_invest_purchases (asset_id)
    WHERE status = 'pending';
CREATE INDEX back_invest_purchases_owner ON back_invest_purchases (owner_id);

ALTER TABLE back_invest_purchases ENABLE ROW LEVEL SECURITY;
