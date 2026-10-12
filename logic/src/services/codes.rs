use uuid::Uuid;

// v4 UUIDs come from the OS CSPRNG (122 random bits), so they double as the random source here.
fn random_u128() -> u128 {
    u128::from_le_bytes(*Uuid::new_v4().as_bytes())
}

// Pickup, drop-off and return codes: 6 digits, matching the app's OTP inputs.
pub fn confirmation_code() -> String {
    format!("{:06}", random_u128() % 1_000_000)
}

// A confirmation code that matches none of `taken`, so one order's codes are never interchangeable.
pub fn confirmation_code_except(taken: &[&str]) -> String {
    loop {
        let code = confirmation_code();
        if !taken.contains(&code.as_str()) {
            return code;
        }
    }
}

// (pickup_code, dropoff_code) for one order, always different.
pub fn code_pair() -> (String, String) {
    let pickup = confirmation_code();
    let dropoff = confirmation_code_except(&[&pickup]);
    (pickup, dropoff)
}

// Unambiguous characters only (no 0/O, 1/I/L), since people read order codes aloud.
const ORDER_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

// Short human-facing order code, e.g. "K7PX3QMA". Unique in the DB; callers retry on a clash.
pub fn order_code() -> String {
    let mut n = random_u128();
    let base = ORDER_CODE_ALPHABET.len() as u128;
    (0..8)
        .map(|_| {
            let c = ORDER_CODE_ALPHABET[(n % base) as usize] as char;
            n /= base;
            c
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_have_expected_shape() {
        for _ in 0..1000 {
            let code = confirmation_code();
            assert_eq!(code.len(), 6);
            assert!(code.chars().all(|c| c.is_ascii_digit()));

            let order = order_code();
            assert_eq!(order.len(), 8);
            assert!(order.bytes().all(|c| ORDER_CODE_ALPHABET.contains(&c)));
        }
    }

    #[test]
    fn order_codes_never_repeat_within_an_order() {
        for _ in 0..1000 {
            let (pickup, dropoff) = code_pair();
            assert_ne!(pickup, dropoff);
            let ret = confirmation_code_except(&[&pickup, &dropoff]);
            assert_ne!(ret, pickup);
            assert_ne!(ret, dropoff);
        }
    }
}
