// Mirrors customer_app/lib/phone.ts normalizePhone, so both sides store the same E.164 string.
// Returns None for anything that doesn't parse as a phone number.
pub fn normalize_phone(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.'))
        .collect();

    let digits_only = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());

    let e164 = if let Some(rest) = cleaned.strip_prefix('+') {
        if !digits_only(rest) { return None; }
        cleaned.clone()
    } else if let Some(rest) = cleaned.strip_prefix("00") {
        if !digits_only(rest) { return None; }
        format!("+{}", rest)
    } else if !digits_only(&cleaned) {
        return None;
    } else if cleaned.starts_with("234") {
        format!("+{}", cleaned)
    } else if cleaned.len() == 11 && cleaned.starts_with('0') {
        format!("+234{}", &cleaned[1..])
    } else if cleaned.len() == 10 && matches!(cleaned.as_bytes()[0], b'7' | b'8' | b'9') {
        format!("+234{}", cleaned)
    } else {
        return None;
    };

    // Nigerian numbers: +234 then a 7/8/9 prefix and 9 more digits.
    if let Some(local) = e164.strip_prefix("+234") {
        let valid = local.len() == 10
            && matches!(local.as_bytes()[0], b'7' | b'8' | b'9')
            && local.bytes().all(|b| b.is_ascii_digit());
        if !valid { return None; }
    }

    // Same rule as the app_custom_users.phone CHECK: ^\+[1-9][0-9]{7,14}$
    let body = &e164[1..];
    let valid = (8..=15).contains(&body.len()) && body.as_bytes()[0] != b'0';
    valid.then_some(e164)
}

#[cfg(test)]
mod tests {
    use super::normalize_phone;

    #[test]
    fn normalizes_nigerian_formats() {
        for raw in ["08031234567", "8031234567", "2348031234567", "+234 803 123 4567", "00234-803-123-4567", "(0803) 123.4567"] {
            assert_eq!(normalize_phone(raw).as_deref(), Some("+2348031234567"), "{}", raw);
        }
    }

    #[test]
    fn keeps_foreign_e164() {
        assert_eq!(normalize_phone("+44 7911 123456").as_deref(), Some("+447911123456"));
    }

    #[test]
    fn rejects_garbage() {
        for raw in ["", "abc", "+2340123456789", "0803123456", "+0123456789", "12345", "+234803123456a"] {
            assert_eq!(normalize_phone(raw), None, "{}", raw);
        }
    }
}
