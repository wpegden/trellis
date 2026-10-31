pub fn add_offset(value: i16, offset: i16) -> i16 {
    value.checked_add(offset).unwrap_or(value)
}

pub fn select_larger(left: u64, right: u64) -> u64 {
    if left >= right { left } else { right }
}

pub fn mutate(values: &mut [u8]) -> usize {
    if let Some(first) = values.first_mut() {
        *first = first.wrapping_add(1);
    }
    values.len()
}

