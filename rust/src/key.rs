pub fn mixed_radix_key(values: &[(usize, u64)], cardinalities: &[u64]) -> Option<u64> {
    let mut key = 0u64;
    for &(column, value) in values {
        let radix = *cardinalities.get(column)?;
        if value >= radix { return None; }
        key = key.checked_mul(radix)?.checked_add(value)?;
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_is_deterministic() {
        let c=[8,12,6];
        assert_eq!(mixed_radix_key(&[(0,3),(1,7)],&c),Some(43));
        assert_eq!(mixed_radix_key(&[(0,3),(1,7)],&c),Some(43));
    }
    #[test]
    fn rejects_out_of_range() { assert_eq!(mixed_radix_key(&[(0,8)],&[8]),None); }
}
