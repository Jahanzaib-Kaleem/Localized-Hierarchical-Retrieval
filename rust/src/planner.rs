use crate::builder::HierarchySpec;

fn combinations(n: usize, width: usize, start: usize, current: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
    if current.len() == width { out.push(current.clone()); return; }
    let need = width - current.len();
    for c in start..=n.saturating_sub(need) {
        current.push(c); combinations(n, width, c + 1, current, out); current.pop();
    }
}

pub fn choose_hierarchies(cardinalities: &[u64], max_width: usize, max_hierarchies: usize, max_keyspace: u64) -> Vec<HierarchySpec> {
    let mut candidates: Vec<(u128, u64, Vec<usize>)> = Vec::new();
    for width in 2..=max_width.min(cardinalities.len()) {
        let mut combos = Vec::new(); combinations(cardinalities.len(), width, 0, &mut Vec::new(), &mut combos);
        for cols in combos {
            let mut space = 1u64; let mut valid = true;
            for &c in &cols { match space.checked_mul(cardinalities[c]) { Some(v) if v <= max_keyspace => space = v, _ => { valid = false; break; } } }
            if valid { candidates.push((space as u128 * 1_000_000u128 / width as u128, space, cols)); }
        }
    }
    candidates.sort_by(|a,b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2)));
    candidates.into_iter().take(max_hierarchies).map(|(_,_,columns)| HierarchySpec { columns }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn planner_is_deterministic_and_bounded() {
        let a = choose_hierarchies(&[8,12,6,16,10], 3, 7, 10_000);
        let b = choose_hierarchies(&[8,12,6,16,10], 3, 7, 10_000);
        assert_eq!(a.len(), 7);
        assert_eq!(a.iter().map(|x| &x.columns).collect::<Vec<_>>(), b.iter().map(|x| &x.columns).collect::<Vec<_>>());
        assert!(a.iter().all(|h| (2..=3).contains(&h.columns.len())));
    }
}
