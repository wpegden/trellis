pub fn tick(word: u16) -> u16 {
    (word & 0xff00) | ((word as u8).wrapping_add(1) as u16)
}
