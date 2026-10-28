-- A driver refuses a package at the pickup (too heavy, too big, prohibited...). The order is
-- cancelled on the spot and the whole charge is held; management reviews (api::disputes):
--   upheld    the driver was right: the driver earns 20% of the charge, the customer is
--             refunded the rest
--   rejected  the driver was at fault: the customer is refunded in full, the driver gets the
--             penalty the reviewer set
-- The customer can read their order's dispute but never change it.
CREATE TABLE back_package_disputes (
    id              BIGSERIAL PRIMARY KEY,
    order_id        BIGINT NOT NULL UNIQUE,
    driver_id       UUID NOT NULL,
    reason          TEXT NOT NULL CHECK (reason IN (
                      'dimensions_exceeded', 'weight_exceeded', 'prohibited_contents',
                      'package_not_found', 'wrong_location')),
    photo_url       TEXT NOT NULL,
    driver_note     TEXT,
    status          TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'upheld', 'rejected')),
    -- Set at review, kobo.
    paid_amount     BIGINT CHECK (paid_amount >= 0),
    driver_amount   BIGINT CHECK (driver_amount >= 0),
    refund_amount   BIGINT CHECK (refund_amount >= 0),
    -- none: nothing to refund (Zazu orders, nothing paid). sent: Korapay took it.
    refund_status   TEXT CHECK (refund_status IN ('pending', 'sent', 'failed', 'none')),
    refund_error    TEXT,
    penalty         BIGINT CHECK (penalty >= 0),
    review_note     TEXT,
    reviewed_by     TEXT,
    reviewed_at     TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX back_package_disputes_pending ON back_package_disputes (created_at) WHERE status = 'pending';
CREATE INDEX back_package_disputes_driver ON back_package_disputes (driver_id, created_at);

-- Money owed to (+) or by (-) a driver outside trip payouts, kobo: dispute compensation and
-- penalties. Netted once a month: a positive total is paid out, a negative one is withheld
-- from later trip payouts. settled_at marks rows already netted.
CREATE TABLE back_driver_adjustments (
    id          BIGSERIAL PRIMARY KEY,
    driver_id   UUID NOT NULL,
    amount      BIGINT NOT NULL CHECK (amount <> 0),
    kind        TEXT NOT NULL CHECK (kind IN ('dispute_compensation', 'dispute_penalty')),
    dispute_id  BIGINT REFERENCES back_package_disputes (id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    settled_at  TIMESTAMPTZ,
    UNIQUE (dispute_id, kind)
);

CREATE INDEX back_driver_adjustments_open ON back_driver_adjustments (driver_id) WHERE settled_at IS NULL;

ALTER TABLE back_package_disputes ENABLE ROW LEVEL SECURITY;
ALTER TABLE back_driver_adjustments ENABLE ROW LEVEL SECURITY;

-- The customer reads the dispute on their own order (read only; review fields stay hidden).
REVOKE ALL ON back_package_disputes FROM anon, authenticated;
GRANT SELECT (id, order_id, reason, photo_url, status, refund_amount, refund_status, reviewed_at, created_at)
    ON back_package_disputes TO authenticated;
CREATE POLICY "Customers read disputes on their orders" ON back_package_disputes
    FOR SELECT TO authenticated
    USING (EXISTS (SELECT 1 FROM app_delivery_orders o WHERE o.id = order_id AND o.client_id = auth.uid()));
