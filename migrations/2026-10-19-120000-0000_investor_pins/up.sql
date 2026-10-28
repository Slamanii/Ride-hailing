-- Investors' withdrawal PINs (services::invest). Only an argon2 hash is kept; a PIN is never
-- stored or read back, only checked. Kept out of invest_accounts, which investors can read:
-- a 4-digit PIN's hash in a client's hands is cracked in seconds.
-- failed_attempts / locked_until: 5 wrong PINs lock it for 30 minutes.
CREATE TABLE back_investor_pins (
    user_id         UUID PRIMARY KEY,
    pin_hash        TEXT NOT NULL,
    failed_attempts INTEGER NOT NULL DEFAULT 0 CHECK (failed_attempts >= 0),
    locked_until    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- No policies: only the backend reads or writes it.
ALTER TABLE back_investor_pins ENABLE ROW LEVEL SECURITY;
