-- Who may use the admin endpoints and dashboard (services::auth::AdminUser).
--   superior  exactly one; grants and revokes admins. The API can never grant, revoke or
--             change it: only a DB update (or a migration) can.
--   admin     granted and revoked by the superior through /admin/admins.
CREATE TABLE back_admins (
    user_id     UUID PRIMARY KEY REFERENCES auth.users (id) ON DELETE CASCADE,
    role        TEXT NOT NULL CHECK (role IN ('superior', 'admin')),
    granted_by  UUID,
    granted_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- At most one superior.
CREATE UNIQUE INDEX back_admins_one_superior ON back_admins ((role)) WHERE role = 'superior';

-- No policies: only the backend reads it.
ALTER TABLE back_admins ENABLE ROW LEVEL SECURITY;

-- The superior. Inserts nothing if the account doesn't exist yet; then run this INSERT again
-- once it does.
INSERT INTO back_admins (user_id, role)
SELECT id, 'superior' FROM auth.users WHERE lower(email) = 'adrianovwigho06@gmail.com'
ON CONFLICT DO NOTHING;
